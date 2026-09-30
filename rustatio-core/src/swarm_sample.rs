//! One-shot swarm completion sampler (v1).
//!
//! Connects to a few peers, reads their bitfields, disconnects.
//! Never requests pieces, never stays connected: the naive pattern with
//! no reciprocal obligations. All tunables live in [`SwarmSampleConfig`]
//! with hardcoded sane defaults so they can be plumbed into
//! `FakerConfig` later without touching call sites.

#[cfg(not(target_arch = "wasm32"))]
use crate::log_debug;
#[cfg(not(target_arch = "wasm32"))]
use crate::protocol::peer::{HANDSHAKE_LEN, INFO_HASH_LEN, PEER_ID_LEN};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
#[cfg(not(target_arch = "wasm32"))]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(not(target_arch = "wasm32"))]
use tokio::net::TcpStream;

/// Tunables for the sampler. Hardcoded defaults for now; a future change
/// can expose these through `FakerConfig` / presets / UI.
#[derive(Debug, Clone)]
pub struct SwarmSampleConfig {
    /// Max peers sampled per cycle (keeps exposure modest).
    pub max_peers: usize,
    /// TCP connect timeout per peer.
    pub connect_timeout: Duration,
    /// Total budget per peer (handshake + bitfield read).
    pub per_peer_timeout: Duration,
    /// Minimum gap between sample cycles.
    pub resample_interval: Duration,
    /// Snapshot freshness budget (default ~2x resample interval).
    pub max_snapshot_age_secs: u64,
    /// Samples at/above this % are seeds/liars, excluded from consensus.
    pub seed_exclude_percent: f64,
    /// Peers above median + band are trimmed as suspicious highs.
    pub trim_band: f64,
    /// Noise floor of the throttle band (percentage points).
    pub epsilon_min_percent: f64,
    /// EMA weight for the swarm velocity estimate.
    pub velocity_alpha: f64,
    /// Safety factor: band covers k full cycles of swarm motion.
    pub velocity_k: f64,
    /// Consensus history ring length.
    pub history_len: usize,
    /// Our completion % at/above which the completion-phase hatch may fire.
    pub phase_min_ours_percent: f64,
    /// Tracker seed fraction at/above which the swarm counts as completing.
    pub phase_min_seed_fraction: f64,
}

impl Default for SwarmSampleConfig {
    fn default() -> Self {
        Self {
            max_peers: 8,
            connect_timeout: Duration::from_secs(5),
            per_peer_timeout: Duration::from_secs(10),
            resample_interval: Duration::from_secs(120),
            max_snapshot_age_secs: 240,
            seed_exclude_percent: 99.5,
            trim_band: 5.0,
            epsilon_min_percent: 1.0,
            velocity_alpha: 0.4,
            velocity_k: 1.5,
            history_len: 4,
            phase_min_ours_percent: 90.0,
            phase_min_seed_fraction: 0.7,
        }
    }
}

/// One peer's observed completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerSample {
    pub addr: String,
    pub peer_id_hex: Option<String>,
    pub percent: f64,
    pub sampled_at_unix: u64,
}

/// Latest sampler output. Empty peers + None timestamp = never sampled.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SwarmSnapshot {
    pub sampled_at_unix: Option<u64>,
    pub peers: Vec<PeerSample>,
    /// Trimmed-median consensus % from the latest cycle, if any.
    #[serde(default)]
    pub consensus_percent: Option<f64>,
    /// Our completion % when the snapshot was last served.
    #[serde(default)]
    pub our_completion_percent: Option<f64>,
    /// Whether pacing is currently throttling download.
    #[serde(default)]
    pub paced: bool,
    /// Seconds since the last sample, computed at access (None if never).
    #[serde(default)]
    pub age_secs: Option<u64>,
    /// True when the snapshot is older than the freshness budget.
    #[serde(default)]
    pub stale: bool,
    /// Swarm velocity estimate (%/s) behind the consensus.
    #[serde(default)]
    pub velocity_pct_per_s: Option<f64>,
    /// Whether the sampler gate is currently satisfied.
    #[serde(default)]
    pub gate_active: bool,
    /// Human-readable state: gate-off reason, or pacing mode when active.
    #[serde(default)]
    pub gate_reason: String,
}

/// Rotating window into the announce peer list: the `offset` advances each
/// cycle so repeated sampling walks the whole list instead of redialing the
/// same (often unreachable) head of it.
pub fn rotate_take(peers: &[SocketAddr], offset: usize, n: usize) -> Vec<SocketAddr> {
    if peers.is_empty() || n == 0 {
        return Vec::new();
    }
    peers.iter().cycle().skip(offset % peers.len()).take(n).copied().collect()
}

/// Human-readable gate state for the API: the first unsatisfied gate
/// condition wins, so `paced=false` is never ambiguous about *why* the
/// mechanism is off. Pure for unit tests.
pub fn describe_gate(
    completion: f64,
    download_intent: f64,
    seeders: i64,
    has_peers: bool,
    snapshot_stale: bool,
    paced: bool,
    pacing_reason: Option<&str>,
) -> (bool, String) {
    if completion >= 100.0 {
        (false, "complete".to_string())
    } else if download_intent <= 0.0 {
        (false, "not downloading".to_string())
    } else if seeders != 1 {
        (false, format!("{seeders} seeders (need exactly 1)"))
    } else if !has_peers {
        (false, "no peers from tracker yet".to_string())
    } else if snapshot_stale {
        (false, "no fresh swarm sample".to_string())
    } else if paced {
        (true, pacing_reason.unwrap_or("pacing").to_string())
    } else {
        (true, "tracking lone-seeder swarm".to_string())
    }
}

pub fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Sample up to `config.max_peers` of `peers`: handshake, read bitfield,
/// disconnect. Returns one entry per peer that served a bitfield.
#[cfg(not(target_arch = "wasm32"))]
pub async fn sample_swarm(
    peers: &[SocketAddr],
    info_hash: [u8; INFO_HASH_LEN],
    our_peer_id: [u8; PEER_ID_LEN],
    total_pieces: u64,
    config: SwarmSampleConfig,
) -> Vec<PeerSample> {
    log_debug!(
        "Swarm sample start: dialing {} of {} known peers",
        config.max_peers.min(peers.len()),
        peers.len()
    );
    let mut set = tokio::task::JoinSet::new();
    for addr in peers.iter().take(config.max_peers) {
        let addr = *addr;
        let cfg = config.clone();
        set.spawn(async move {
            tokio::time::timeout(
                cfg.per_peer_timeout,
                sample_one_peer(addr, info_hash, our_peer_id, total_pieces, &cfg),
            )
            .await
            .ok()
            .flatten()
        });
    }
    let mut out = Vec::new();
    while let Some(res) = set.join_next().await {
        if let Ok(Some(sample)) = res {
            out.push(sample);
        }
    }
    log_debug!("Swarm sample done: {} peers served bitfields", out.len());
    out
}

#[cfg(not(target_arch = "wasm32"))]
async fn sample_one_peer(
    addr: SocketAddr,
    info_hash: [u8; INFO_HASH_LEN],
    our_peer_id: [u8; PEER_ID_LEN],
    total_pieces: u64,
    config: &SwarmSampleConfig,
) -> Option<PeerSample> {
    if total_pieces == 0 {
        log_debug!("Swarm dial {addr}: no pieces in torrent, skip");
        return None;
    }
    log_debug!("Swarm dial {addr}: connecting");
    let mut stream =
        match tokio::time::timeout(config.connect_timeout, TcpStream::connect(addr)).await {
            Ok(Ok(s)) => {
                log_debug!("Swarm peer {addr}: connected, sending handshake");
                s
            }
            _ => {
                log_debug!("Swarm dial {addr}: connect failed or timed out");
                return None;
            }
        };

    // 68-byte handshake: len + protocol + reserved + info_hash + peer_id.
    let mut hs = [0u8; HANDSHAKE_LEN];
    hs[0] = 19;
    hs[1..20].copy_from_slice(b"BitTorrent protocol");
    hs[28..48].copy_from_slice(&info_hash);
    hs[48..68].copy_from_slice(&our_peer_id);
    if stream.write_all(&hs).await.is_err() {
        log_debug!("Swarm peer {addr}: handshake send failed");
        return None;
    }
    log_debug!("Swarm peer {addr}: handshake sent, waiting for reply");

    let mut reply = [0u8; HANDSHAKE_LEN];
    if stream.read_exact(&mut reply).await.is_err() {
        log_debug!("Swarm peer {addr}: no handshake reply");
        return None;
    }
    if reply[28..48] != info_hash {
        log_debug!("Swarm peer {addr}: info-hash mismatch, disconnect");
        return None;
    }
    log_debug!("Swarm peer {addr}: handshake ok, waiting for bitfield");
    let peer_id_hex = Some(to_hex(&reply[48..68]));

    // Frames until the first bitfield (id 5); skip anything else.
    loop {
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await.ok()?;
        let len = u32::from_be_bytes(len_buf);
        if len == 0 {
            continue; // keep-alive
        }
        if len > 4 * 1024 * 1024 {
            log_debug!("Swarm peer {addr}: absurd frame len {len}, bail");
            return None;
        }
        let mut id_buf = [0u8; 1];
        stream.read_exact(&mut id_buf).await.ok()?;
        let rest = (len - 1) as usize;
        if id_buf[0] == 5 {
            let mut bitfield = vec![0u8; rest];
            if stream.read_exact(&mut bitfield).await.is_err() {
                log_debug!("Swarm peer {addr}: bitfield read failed");
                return None;
            }
            let have: u32 = bitfield.iter().map(|b| b.count_ones()).sum();
            let percent = (have as f64 / total_pieces as f64 * 100.0).clamp(0.0, 100.0);
            log_debug!("Swarm peer {addr}: bitfield {percent:.1}%");
            return Some(PeerSample {
                addr: addr.to_string(),
                peer_id_hex,
                percent,
                sampled_at_unix: now_unix(),
            });
        }
        // Skip payload of uninteresting frames without allocating big bufs.
        let mut remaining = rest;
        let mut sink = [0u8; 4096];
        while remaining > 0 {
            let n = remaining.min(sink.len());
            stream.read_exact(&mut sink[..n]).await.ok()?;
            remaining -= n;
        }
    }
}

/// Throttle decision inputs: all plain data, no I/O.
#[derive(Debug, Clone)]
pub struct PaceDecision {
    /// None = full configured speed; Some(v) = cap download at v KB/s
    /// (0 = hold while ahead of the swarm).
    pub capped_rate: Option<f64>,
    /// True when the completion-phase hatch fired (finish like the seeds).
    pub phase_finish: bool,
    /// Latest trimmed-median consensus %.
    pub consensus: f64,
    /// Consensus projected to now via the velocity estimate.
    pub projected: f64,
    /// Hysteresis band in percentage points.
    pub delta: f64,
}

/// Median peer completion after exclusions: drop seeds/liars at or above
/// the seed threshold, take the median, then trim suspicious highs above
/// median + band and re-take the median. None when nothing usable remains.
pub fn trimmed_median(samples: &[PeerSample], config: &SwarmSampleConfig) -> Option<f64> {
    let mut vals: Vec<f64> =
        samples.iter().map(|s| s.percent).filter(|p| *p < config.seed_exclude_percent).collect();
    if vals.is_empty() {
        return None;
    }
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = percentile(&vals, 50.0);
    vals.retain(|p| *p <= median + config.trim_band);
    if vals.is_empty() {
        return None;
    }
    Some(percentile(&vals, 50.0))
}

fn percentile(sorted: &[f64], percent: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    let rank = percent / 100.0 * (sorted.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    sorted[lo] + (sorted[hi] - sorted[lo]) * (rank - lo as f64)
}

/// Pace law with three zones: full configured speed while at or behind the
/// projected swarm P, swarm-speed matching inside the band (P, P+delta],
/// full hold beyond it. The cap is never a boost: the caller clamps it to
/// its configured rate. None with no consensus history (fail open). The
/// band covers a full cycle of estimated swarm motion so the decision line
/// moves with the swarm instead of sawtoothing 0/max around a static one.
pub fn pace_decision(
    our_completion: f64,
    history: &[(u64, f64)],
    velocity: f64,
    now: u64,
    total_pieces: u64,
    total_size_bytes: u64,
    seed_fraction: Option<f64>,
    config: &SwarmSampleConfig,
) -> Option<PaceDecision> {
    let (at, consensus) = history.last().copied()?;
    let speed = velocity.max(0.0);
    let age = now.saturating_sub(at) as f64;
    let projected = consensus + speed * age;
    let one_piece = if total_pieces > 0 { 100.0 / total_pieces as f64 } else { 0.0 };
    let delta = config
        .epsilon_min_percent
        .max(one_piece)
        .max(config.velocity_k * speed * config.resample_interval.as_secs_f64());
    // Completion-phase escape hatch: far along ourselves in a mostly-seed
    // swarm, finish at full speed like everyone else did. One-way per
    // torrent, so no oscillation risk. Liar-safe: fake seeds inflate the
    // fraction, but young swarms have large leecher denominators and mid-O
    // torrents never reach the ours threshold.
    let phase_finish = seed_fraction
        .map(|s| {
            our_completion >= config.phase_min_ours_percent && s >= config.phase_min_seed_fraction
        })
        .unwrap_or(false);
    if phase_finish {
        return Some(PaceDecision {
            consensus,
            projected,
            delta,
            capped_rate: None,
            phase_finish: true,
        });
    }
    let capped_rate = if our_completion <= projected {
        None
    } else if our_completion > projected + delta {
        Some(0.0)
    } else {
        // Inside the band: ride at the swarm's own speed in KB/s.
        Some(speed / 100.0 * total_size_bytes as f64 / 1024.0)
    };
    Some(PaceDecision { consensus, projected, delta, capped_rate, phase_finish: false })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_math_edges() {
        // 8 pieces, first 3 set -> 37.5%.
        let bits = [0b1110_0000u8];
        let have: u32 = bits.iter().map(|b| b.count_ones()).sum();
        let pct = have as f64 / 8.0 * 100.0;
        assert!((pct - 37.5).abs() < f64::EPSILON);
    }

    fn sample(pct: f64) -> PeerSample {
        PeerSample {
            addr: "1.2.3.4:6881".to_string(),
            peer_id_hex: None,
            percent: pct,
            sampled_at_unix: 1,
        }
    }

    #[test]
    fn gate_reason_names_first_failure() {
        // Complete beats everything else.
        assert_eq!(describe_gate(100.0, 0.0, 5, false, true, false, None).0, false);
        assert_eq!(describe_gate(100.0, 0.0, 5, false, true, false, None).1, "complete");
        // Order: downloading -> lone seeder -> peers -> freshness.
        assert_eq!(describe_gate(20.0, 0.0, 1, true, false, false, None).1, "not downloading");
        assert_eq!(
            describe_gate(20.0, 5.0, 3, true, false, false, None).1,
            "3 seeders (need exactly 1)"
        );
        assert_eq!(
            describe_gate(20.0, 5.0, 1, false, false, false, None).1,
            "no peers from tracker yet"
        );
        assert_eq!(describe_gate(20.0, 5.0, 1, true, true, false, None).1, "no fresh swarm sample");
        // Active reveals pacing mode, not just a boolean.
        let (on, why) = describe_gate(
            20.0,
            5.0,
            1,
            true,
            false,
            true,
            Some("matching swarm speed (20.0% vs 4.4%)"),
        );
        assert!(on);
        assert!(why.contains("matching"));
        let (on, why) = describe_gate(20.0, 5.0, 1, true, false, false, None);
        assert!(on);
        assert_eq!(why, "tracking lone-seeder swarm");
    }

    #[test]
    fn rotate_take_walks_the_list() {
        let addrs: Vec<SocketAddr> =
            (1..=5).map(|i| format!("10.0.0.{i}:6881").parse().unwrap()).collect();
        assert_eq!(rotate_take(&addrs, 0, 2), addrs[..2]);
        assert_eq!(rotate_take(&addrs, 2, 2), addrs[2..4]);
        // Wraps around the end instead of redialing the head.
        let wrapped = rotate_take(&addrs, 4, 3);
        assert_eq!(wrapped.len(), 3);
        assert_eq!(wrapped[0], addrs[4]);
        assert_eq!(wrapped[1], addrs[0]);
    }

    #[test]
    fn consensus_ignores_seeds_and_trims_liars() {
        let cfg = SwarmSampleConfig::default();
        // Lone seed + hot liar must not move consensus off the pack.
        let peers = [1.5, 1.6, 1.7, 100.0, 45.0].map(sample);
        let c = trimmed_median(&peers, &cfg).unwrap();
        assert!((c - 1.6).abs() < 0.11, "consensus={c}");
    }

    #[test]
    fn consensus_empty_without_usable_peers() {
        let cfg = SwarmSampleConfig::default();
        assert!(trimmed_median(&[], &cfg).is_none());
        assert!(trimmed_median(&[sample(100.0)], &cfg).is_none());
    }

    #[test]
    fn pace_throttles_only_ahead_of_band() {
        let cfg = SwarmSampleConfig::default();
        let size = 1_000_000_000u64;
        let hist = [(1000u64, 2.0f64)];
        // Behind consensus: full speed.
        let d = pace_decision(1.0, &hist, 0.0, 1010, 1000, size, None, &cfg).unwrap();
        assert!(d.capped_rate.is_none());
        // 5 points ahead with static band (~1% floor): hold.
        let d = pace_decision(7.0, &hist, 0.0, 1010, 1000, size, None, &cfg).unwrap();
        assert_eq!(d.capped_rate, Some(0.0), "delta={}", d.delta);
        // No history: fail open.
        assert!(pace_decision(99.0, &[], 0.0, 1010, 1000, size, None, &cfg).is_none());
    }

    #[test]
    fn pace_matches_swarm_speed_inside_band() {
        let cfg = SwarmSampleConfig::default();
        // 1 GiB torrent, swarm at 0.01 %/s -> ~105 KB/s match rate.
        let size = 1_073_741_824u64;
        let hist = [(1000u64, 2.0f64)];
        // O=3.0 sits inside (P=2.1, P+delta=3.9]: ride, don't sawtooth.
        let d = pace_decision(3.0, &hist, 0.01, 1010, 10000, size, None, &cfg).unwrap();
        let cap = d.capped_rate.unwrap();
        assert!((cap - 104.9).abs() < 0.5, "cap={cap}");
        // Unknown velocity inside the band: hold rather than overshoot.
        let d = pace_decision(3.0, &hist, 0.0, 1010, 10000, size, None, &cfg).unwrap();
        assert_eq!(d.capped_rate, Some(0.0));
    }

    #[test]
    fn pace_projection_tracks_fast_swarm() {
        let cfg = SwarmSampleConfig::default();
        // Swarm moving 0.1 %/s; after 60 s it moved 6 points: we are
        // level with the projection, so no throttle despite leading
        // the last static consensus.
        let hist = [(1000u64, 2.0f64), (1060u64, 8.0f64)];
        let size = 1_000_000_000u64;
        let d = pace_decision(13.0, &hist, 0.1, 1120, 10000, size, None, &cfg).unwrap();
        assert!(d.capped_rate.is_none(), "projected={} delta={}", d.projected, d.delta);
        // Stalled swarm: same position holds.
        let d = pace_decision(13.0, &hist, 0.0, 1120, 10000, size, None, &cfg).unwrap();
        assert_eq!(d.capped_rate, Some(0.0));
    }

    #[test]
    fn phase_hatch_finishes_only_when_both_halves_hold() {
        let cfg = SwarmSampleConfig::default();
        let size = 1_000_000_000u64;
        let hist = [(1000u64, 30.0f64)];
        // Far along in a mostly-seed swarm: finish.
        let d = pace_decision(95.0, &hist, 0.0, 1010, 1000, size, Some(0.8), &cfg).unwrap();
        assert!(d.phase_finish);
        assert!(d.capped_rate.is_none());
        // Mid completion despite seed-heavy counts: keep pacing.
        let d = pace_decision(40.0, &hist, 0.0, 1010, 1000, size, Some(0.8), &cfg).unwrap();
        assert!(!d.phase_finish);
        // Far along but young swarm (liar-inflated counts): keep pacing.
        let d = pace_decision(95.0, &hist, 0.0, 1010, 1000, size, Some(0.2), &cfg).unwrap();
        assert!(!d.phase_finish);
        assert_eq!(d.capped_rate, Some(0.0));
        // Unknown fraction: never hatch.
        let d = pace_decision(95.0, &hist, 0.0, 1010, 1000, size, None, &cfg).unwrap();
        assert!(!d.phase_finish);
    }

    #[test]
    fn defaults_are_sane() {
        let c = SwarmSampleConfig::default();
        assert!(c.max_peers > 0 && c.max_peers <= 50);
        assert!(c.per_peer_timeout >= c.connect_timeout);
        assert!(c.resample_interval.as_secs() >= 60);
    }
}

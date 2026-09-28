use crate::protocol::bencode;
use crate::torrent::ClientConfig;
use crate::{log_debug, log_error, log_info, log_trace, log_warn};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::fmt::Write;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TrackerError {
    #[error("HTTP error: {0}")]
    HttpError(String),
    #[error("Bencode error: {0}")]
    BencodeError(#[from] crate::protocol::bencode::BencodeError),
    #[error("Tracker returned error: {0}")]
    TrackerFailure(String),
    #[error("Invalid response: {0}")]
    InvalidResponse(String),
    #[error("URL parse error: {0}")]
    UrlError(#[from] url::ParseError),
}

impl From<reqwest::Error> for TrackerError {
    fn from(err: reqwest::Error) -> Self {
        Self::HttpError(err.to_string())
    }
}

pub type Result<T> = std::result::Result<T, TrackerError>;

pub type HttpResult = std::result::Result<HttpResponse, String>;

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(test, mockall::automock)]
pub trait HttpClient: Send + Sync {
    async fn get(&self, url: String, agent: String) -> HttpResult;
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    status: StatusCode,
    body: Vec<u8>,
}

pub struct ReqwestHttpClient {
    client: reqwest::Client,
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl HttpClient for ReqwestHttpClient {
    async fn get(&self, url: String, agent: String) -> HttpResult {
        let req = self.client.get(url).header(reqwest::header::USER_AGENT, agent);
        let res = req.send().await.map_err(|err| err.to_string())?;
        let status = res.status();
        let body = res.bytes().await.map_err(|err| err.to_string())?;
        Ok(HttpResponse { status, body: body.to_vec() })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TrackerEvent {
    Started,
    Stopped,
    Completed,
    None,
}

impl TrackerEvent {
    pub const fn as_str(&self) -> Option<&str> {
        match self {
            Self::Started => Some("started"),
            Self::Stopped => Some("stopped"),
            Self::Completed => Some("completed"),
            Self::None => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AnnounceRequest {
    pub info_hash: [u8; 20],
    pub peer_id: String,
    pub port: u16,
    pub uploaded: u64,
    pub downloaded: u64,
    pub left: u64,
    pub compact: bool,
    pub no_peer_id: bool,
    pub event: TrackerEvent,
    pub ip: Option<String>,
    pub ipv4: Option<String>,
    pub ipv6: Option<String>,
    pub numwant: Option<u32>,
    pub key: Option<String>,
    pub tracker_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnounceResponse {
    /// Interval in seconds between announces
    pub interval: i64,

    /// Minimum announce interval
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_interval: Option<i64>,

    /// Tracker ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tracker_id: Option<String>,

    /// Number of seeders
    pub complete: i64,

    /// Number of leechers
    pub incomplete: i64,

    /// Warning message
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,

    /// Peers from this announce (compact or dict form), may be empty
    #[serde(default)]
    pub peers: Vec<SocketAddr>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScrapeResponse {
    pub complete: i64,
    pub incomplete: i64,
    pub downloaded: i64,
    pub name: Option<String>,
}

/// Our egress addresses toward a tracker, discovered by UDP route lookup
/// (connect() sends no packets; it only resolves the egress interface).
/// Returns (ipv4, ipv6); a family without a route resolves to None so the
/// caller omits that announce param. No external services involved.
pub fn egress_addrs_for_tracker(tracker_url: &str) -> (Option<String>, Option<String>) {
    let host = tracker_host(tracker_url);
    if host.is_empty() {
        return (None, None);
    }
    let probe = |bind: &str| {
        let sock = std::net::UdpSocket::bind(bind).ok()?;
        sock.connect((host, 80)).ok()?;
        let ip = sock.local_addr().ok()?.ip();
        if !is_public_ip(&ip) {
            return None;
        }
        Some(ip.to_string())
    };
    (probe("0.0.0.0:0"), probe("[::]:0"))
}

/// Globally routable addresses only: never announce private, loopback,
/// link-local, unspecified, multicast, or documentation ranges.
fn is_public_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v) => {
            !v.is_private()
                && !v.is_loopback()
                && !v.is_link_local()
                && !v.is_broadcast()
                && !v.is_documentation()
                && !v.is_unspecified()
        }
        std::net::IpAddr::V6(v) => {
            let segs = v.segments();
            !v.is_loopback()
                && !v.is_unspecified()
                && !v.is_multicast()
                && (segs[0] & 0xffc0) != 0xfe80 // unicast link-local fe80::/10
                && (segs[0] & 0xfe00) != 0xfc00 // unique local fc00::/7
                && !(segs[0] == 0x2001 && segs[1] == 0x0db8) // documentation 2001:db8::/32
        }
    }
}

/// Helper services reporting our public address as seen from the internet.
/// Bodies are CSV (`IPv4,203.0.113.7,v1.1,...`); the address is the second
/// field. Queried only for families without a public egress route.
const IPV4_PROBE_URL: &str = "https://ip4only.me/api/";
const IPV6_PROBE_URL: &str = "https://ip6only.me/api/";

/// Best-effort public addresses for families lacking a public egress route.
/// Returns (ipv4, ipv6) for the caller to cache; any failure resolves to
/// None so the param is omitted rather than guessed.
pub async fn probe_public_addrs(tracker_url: &str) -> (Option<String>, Option<String>) {
    let (local_v4, local_v6) = egress_addrs_for_tracker(tracker_url);
    let builder = reqwest::Client::builder();
    #[cfg(not(target_arch = "wasm32"))]
    let builder = builder.timeout(std::time::Duration::from_secs(8));
    let client = match builder.build() {
        Ok(c) => c,
        Err(_) => return (None, None),
    };
    let v4 =
        if local_v4.is_none() { probe_one(&client, IPV4_PROBE_URL, false).await } else { None };
    let v6 = if local_v6.is_none() { probe_one(&client, IPV6_PROBE_URL, true).await } else { None };
    (v4, v6)
}

async fn probe_one(client: &reqwest::Client, url: &str, want_v6: bool) -> Option<String> {
    let text = client.get(url).send().await.ok()?.text().await.ok()?;
    parse_probe_body(&text, want_v6)
}

fn parse_probe_body(text: &str, want_v6: bool) -> Option<String> {
    // Plain IP, or CSV with the address in the second field.
    let candidate = text.split(',').nth(1).unwrap_or(text);
    let ip: std::net::IpAddr = candidate.trim().parse().ok()?;
    if ip.is_ipv6() != want_v6 || !is_public_ip(&ip) {
        return None;
    }
    Some(ip.to_string())
}

fn tracker_host(tracker_url: &str) -> &str {
    let after_scheme = tracker_url.split("://").nth(1).unwrap_or(tracker_url);
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.split('@').next_back().unwrap_or(authority);
    if let Some(rest) = authority.strip_prefix('[') {
        let end = rest.find(']').unwrap_or(rest.len());
        &rest[..end]
    } else {
        authority.split(':').next().unwrap_or(authority)
    }
}

pub struct TrackerClient<C: HttpClient = ReqwestHttpClient> {
    http: C,
    client_config: ClientConfig,
}

impl<C: HttpClient> TrackerClient<C> {
    #[cfg(target_arch = "wasm32")]
    fn proxy_url() -> Option<String> {
        let window = web_sys::window()?;
        let storage = window.local_storage().ok()??;
        let proxy = storage.get_item("rustatio-proxy-url").ok()??;
        if proxy.is_empty() {
            None
        } else {
            Some(proxy)
        }
    }

    /// Send an announce request to the tracker
    pub async fn announce(
        &self,
        tracker_url: &str,
        request: &AnnounceRequest,
    ) -> Result<AnnounceResponse> {
        let announce_url = self.build_announce_url(tracker_url, request);

        // For WASM, check if proxy is configured
        #[cfg(target_arch = "wasm32")]
        let final_url = if let Some(proxy) = Self::proxy_url() {
            let encoded = percent_encoding::utf8_percent_encode(
                &announce_url,
                percent_encoding::NON_ALPHANUMERIC,
            )
            .to_string();
            format!("{}?url={}", proxy.trim_end_matches('/'), encoded)
        } else {
            announce_url.clone()
        };

        #[cfg(not(target_arch = "wasm32"))]
        let final_url = announce_url.clone();

        log_info!("Announcing to tracker: {}", tracker_url);
        log_debug!("Full announce URL: {}", final_url);

        let response = self
            .http
            .get(final_url, self.client_config.user_agent.clone())
            .await
            .map_err(TrackerError::HttpError)?;

        let status = response.status;
        log_trace!("Tracker response status: {}", status);

        if !status.is_success() {
            log_error!("Tracker request failed with status: {}", status);
            return Err(TrackerError::HttpError(format!("HTTP status: {status}")));
        }

        let body = response.body;
        log_debug!("Tracker response: {} bytes", body.len());
        log_trace!("Response body (hex): {:02X?}", &body[..body.len().min(100)]);

        self.parse_announce_response(&body)
    }

    /// Send a scrape request to the tracker
    pub async fn scrape(&self, tracker_url: &str, info_hash: &[u8; 20]) -> Result<ScrapeResponse> {
        let scrape_url = self.build_scrape_url(tracker_url, info_hash);

        log_info!("Scraping tracker: {}", scrape_url);

        let response = self
            .http
            .get(scrape_url, self.client_config.user_agent.clone())
            .await
            .map_err(TrackerError::HttpError)?;

        if !response.status.is_success() {
            return Err(TrackerError::HttpError(format!("HTTP status: {}", response.status)));
        }

        let body = response.body;
        self.parse_scrape_response(&body, info_hash)
    }

    /// Build announce URL with all parameters
    fn build_announce_url(&self, tracker_url: &str, request: &AnnounceRequest) -> String {
        // Build query parameters manually since info_hash needs special encoding
        let info_hash_encoded: String =
            request.info_hash.iter().fold(String::new(), |mut acc, b| {
                let _ = write!(acc, "%{b:02X}");
                acc
            });

        let mut params = vec![
            format!("info_hash={}", info_hash_encoded),
            format!("peer_id={}", request.peer_id),
            format!("port={}", request.port),
            format!("uploaded={}", request.uploaded),
            format!("downloaded={}", request.downloaded),
            format!("left={}", request.left),
            format!("compact={}", if request.compact { "1" } else { "0" }),
        ];

        if request.no_peer_id {
            params.push("no_peer_id=1".to_string());
        }

        if let Some(event) = request.event.as_str() {
            params.push(format!("event={event}"));
        }

        if let Some(ref ip) = request.ip {
            params.push(format!("ip={ip}"));
        }

        if let Some(ref ipv4) = request.ipv4 {
            params.push(format!("ipv4={ipv4}"));
        }

        if let Some(ref ipv6) = request.ipv6 {
            params.push(format!("ipv6={ipv6}"));
        }

        if let Some(numwant) = request.numwant {
            params.push(format!("numwant={numwant}"));
        }

        if let Some(ref key) = request.key {
            params.push(format!("key={key}"));
        }

        if let Some(ref tracker_id) = request.tracker_id {
            params.push(format!("trackerid={tracker_id}"));
        }

        // Add client-specific parameters
        if self.client_config.supports_crypto {
            params.push("supportcrypto=1".to_string());
        }

        let query_string = params.join("&");
        let separator = if tracker_url.contains('?') { '&' } else { '?' };

        format!("{tracker_url}{separator}{query_string}")
    }

    #[allow(clippy::unused_self)]
    fn build_scrape_url(&self, tracker_url: &str, info_hash: &[u8; 20]) -> String {
        // Convert announce URL to scrape URL
        let scrape_url = tracker_url.replace("/announce", "/scrape");

        // URL encode info_hash (same format as announce)
        let info_hash_encoded: String = info_hash.iter().fold(String::new(), |mut acc, b| {
            let _ = write!(acc, "%{b:02X}");
            acc
        });

        // Build URL with query parameter
        let separator = if scrape_url.contains('?') { '&' } else { '?' };
        format!("{scrape_url}{separator}info_hash={info_hash_encoded}")
    }

    /// Parse announce response from bencoded data
    fn parse_announce_response(&self, data: &[u8]) -> Result<AnnounceResponse> {
        log_trace!("Parsing announce response ({} bytes)", data.len());

        let Ok(value) = bencode::parse(data) else {
            // Try to provide a helpful error message about what the tracker returned
            let preview = self.format_response_preview(data);
            log_error!(
                "Failed to parse tracker response as bencode. Response preview: {}",
                preview
            );
            return Err(TrackerError::InvalidResponse(format!(
                "Tracker returned invalid response (not bencode). {preview}. This usually means: invalid passkey, torrent not registered, IP blocked, or tracker requires login."
            )));
        };
        let serde_bencode::value::Value::Dict(dict) = &value else {
            log_error!("Invalid response: not a dictionary");
            return Err(TrackerError::InvalidResponse("Response is not a dictionary".into()));
        };

        // Check for failure
        if let Some(serde_bencode::value::Value::Bytes(bytes)) =
            dict.get(b"failure reason".as_ref())
        {
            let reason = String::from_utf8_lossy(bytes).to_string();
            log_error!("Tracker returned failure: {}", reason);
            return Err(TrackerError::TrackerFailure(reason));
        }

        // Check for warning
        if let Some(serde_bencode::value::Value::Bytes(bytes)) =
            dict.get(b"warning message".as_ref())
        {
            let warning = String::from_utf8_lossy(bytes).to_string();
            log_warn!("Tracker warning: {}", warning);
        }

        // Extract required fields
        let interval = bencode::get_int(dict, "interval")?;
        let complete = bencode::get_int(dict, "complete").unwrap_or(0);
        let incomplete = bencode::get_int(dict, "incomplete").unwrap_or(0);

        log_debug!(
            "Parsed response: interval={}s, seeders={}, leechers={}",
            interval,
            complete,
            incomplete
        );

        // Extract optional fields
        let min_interval = dict.get(b"min interval".as_ref()).and_then(|v| match v {
            serde_bencode::value::Value::Int(i) => Some(*i),
            _ => None,
        });
        let tracker_id = dict.get(b"tracker id".as_ref()).and_then(|v| match v {
            serde_bencode::value::Value::Bytes(b) => Some(String::from_utf8_lossy(b).to_string()),
            _ => None,
        });
        let warning = dict.get(b"warning message".as_ref()).and_then(|v| match v {
            serde_bencode::value::Value::Bytes(b) => Some(String::from_utf8_lossy(b).to_string()),
            _ => None,
        });

        let peers = Self::parse_peers(dict);
        Ok(AnnounceResponse {
            interval,
            min_interval,
            tracker_id,
            complete,
            incomplete,
            warning,
            peers,
        })
    }

    fn parse_peers(
        dict: &std::collections::HashMap<Vec<u8>, serde_bencode::value::Value>,
    ) -> Vec<SocketAddr> {
        let mut out = Vec::new();
        // IPv4 under "peers": compact 6-byte form + dictionary-list form.
        if let Some(raw) = dict.get(b"peers".as_ref()) {
            match raw {
                // Compact form: 6 bytes per peer (4 IP + 2 port BE).
                serde_bencode::value::Value::Bytes(b) => {
                    out.extend(b.chunks_exact(6).filter_map(|c| {
                        let ip = Ipv4Addr::new(c[0], c[1], c[2], c[3]);
                        let port = u16::from_be_bytes([c[4], c[5]]);
                        (port != 0).then(|| SocketAddr::V4(SocketAddrV4::new(ip, port)))
                    }))
                }
                // Dictionary form: [{ip, port}, ...].
                serde_bencode::value::Value::List(peers) => {
                    out.extend(peers.iter().filter_map(|p| {
                        let serde_bencode::value::Value::Dict(d) = p else {
                            return None;
                        };
                        let ip = d.get(b"ip".as_ref()).and_then(|v| match v {
                            serde_bencode::value::Value::Bytes(b) => {
                                String::from_utf8_lossy(b).parse().ok()
                            }
                            _ => None,
                        })?;
                        let port = d.get(b"port".as_ref()).and_then(|v| match v {
                            serde_bencode::value::Value::Int(i) => u16::try_from(*i).ok(),
                            _ => None,
                        })?;
                        (port != 0).then(|| SocketAddr::new(ip, port))
                    }));
                }
                _ => {}
            }
        }
        // IPv6 under "peers6": compact 18 bytes per peer (16 IP + 2 port BE).
        if let Some(serde_bencode::value::Value::Bytes(b)) = dict.get(b"peers6".as_ref()) {
            out.extend(b.chunks_exact(18).filter_map(|c| {
                let ip = Ipv6Addr::from([
                    c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7], c[8], c[9], c[10], c[11],
                    c[12], c[13], c[14], c[15],
                ]);
                let port = u16::from_be_bytes([c[16], c[17]]);
                (port != 0).then(|| SocketAddr::V6(SocketAddrV6::new(ip, port, 0, 0)))
            }));
        }
        out
    }

    /// Parse scrape response from bencoded data
    fn parse_scrape_response(&self, data: &[u8], info_hash: &[u8; 20]) -> Result<ScrapeResponse> {
        let Ok(value) = bencode::parse(data) else {
            let preview = self.format_response_preview(data);
            log_error!("Failed to parse scrape response as bencode. Response preview: {}", preview);
            return Err(TrackerError::InvalidResponse(format!(
                "Tracker returned invalid scrape response (not bencode). {preview}"
            )));
        };
        let serde_bencode::value::Value::Dict(dict) = &value else {
            return Err(TrackerError::InvalidResponse("Response is not a dictionary".into()));
        };

        // Get the files dictionary
        let files = dict
            .get(b"files".as_ref())
            .and_then(|v| match v {
                serde_bencode::value::Value::Dict(d) => Some(d),
                _ => None,
            })
            .ok_or_else(|| {
                TrackerError::InvalidResponse("Missing 'files' in scrape response".into())
            })?;

        // Find our torrent's stats (the key is the raw info_hash bytes)
        let stats = files
            .get(info_hash.as_ref())
            .and_then(|v| match v {
                serde_bencode::value::Value::Dict(d) => Some(d),
                _ => None,
            })
            .ok_or_else(|| {
                TrackerError::InvalidResponse("Torrent not found in scrape response".into())
            })?;

        let complete = bencode::get_int(stats, "complete")?;
        let incomplete = bencode::get_int(stats, "incomplete")?;
        let downloaded = bencode::get_int(stats, "downloaded")?;
        let name = stats.get(b"name".as_ref()).and_then(|v| match v {
            serde_bencode::value::Value::Bytes(b) => Some(String::from_utf8_lossy(b).to_string()),
            _ => None,
        });

        Ok(ScrapeResponse { complete, incomplete, downloaded, name })
    }

    /// Format a preview of the response data for error messages
    #[allow(clippy::unused_self)]
    fn format_response_preview(&self, data: &[u8]) -> String {
        if data.is_empty() {
            return "Response was empty".to_string();
        }

        // Check for common HTML indicators
        let is_html = data.starts_with(b"<!DOCTYPE")
            || data.starts_with(b"<!doctype")
            || data.starts_with(b"<html")
            || data.starts_with(b"<HTML")
            || data.starts_with(b"<?xml");

        // Check for gzip magic bytes
        let is_gzip = data.len() >= 2 && data[0] == 0x1f && data[1] == 0x8b;

        if is_html {
            // Try to extract a meaningful snippet from HTML
            let text = String::from_utf8_lossy(&data[..data.len().min(500)]);
            // Try to find title or error message
            if let Some(start) = text.find("<title>").or_else(|| text.find("<TITLE>")) {
                if let Some(end) =
                    text[start..].find("</title>").or_else(|| text[start..].find("</TITLE>"))
                {
                    let title = &text[start + 7..start + end];
                    return format!("Received HTML page with title: \"{}\"", title.trim());
                }
            }
            return "Received HTML page instead of tracker response".to_string();
        }

        if is_gzip {
            return "Received gzip-compressed response (tracker may require Accept-Encoding header)".to_string();
        }

        // For other binary data, show a preview
        let preview_len = data.len().min(100);
        let text = String::from_utf8_lossy(&data[..preview_len]);

        // If it looks like text, show it
        if text.chars().filter(|c| c.is_ascii_graphic() || c.is_ascii_whitespace()).count()
            > preview_len / 2
        {
            format!(
                "Response starts with: \"{}{}\"",
                text.trim(),
                if data.len() > preview_len { "..." } else { "" }
            )
        } else {
            format!("Received {} bytes of binary data", data.len())
        }
    }
}

impl TrackerClient<ReqwestHttpClient> {
    /// Create a new `TrackerClient`.
    ///
    /// If `shared_client` is provided, it will be reused (saving ~1-5 MB per instance).
    /// User-Agent is set per-request so different instances can emulate different BT clients.
    pub fn new(
        client_config: ClientConfig,
        shared_client: Option<reqwest::Client>,
    ) -> Result<Self> {
        log_debug!("Creating TrackerClient with User-Agent: {}", client_config.user_agent);

        let client = if let Some(c) = shared_client {
            c
        } else {
            #[cfg(not(target_arch = "wasm32"))]
            {
                reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(30))
                    .gzip(true)
                    .build()?
            }

            #[cfg(target_arch = "wasm32")]
            {
                reqwest::Client::builder().build()?
            }
        };

        Ok(Self { http: ReqwestHttpClient { client }, client_config })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::torrent::ClientType;
    use serde_bencode::value::Value;
    use std::collections::HashMap;

    fn client() -> Result<TrackerClient<ReqwestHttpClient>> {
        let cfg = ClientConfig::get(ClientType::QBittorrent, None);
        TrackerClient::new(cfg, None)
    }

    fn hash() -> [u8; 20] {
        let mut hash = [0u8; 20];
        for (idx, item) in hash.iter_mut().enumerate() {
            *item = idx as u8;
        }
        hash
    }

    fn encode_hash(hash: [u8; 20]) -> String {
        let mut out = String::new();
        for byte in hash {
            let _ = write!(out, "%{byte:02X}");
        }
        out
    }

    fn req(hash: [u8; 20]) -> AnnounceRequest {
        AnnounceRequest {
            info_hash: hash,
            peer_id: "peerid12345678901234".to_string(),
            port: 6881,
            uploaded: 123,
            downloaded: 456,
            left: 789,
            compact: true,
            no_peer_id: true,
            event: TrackerEvent::Started,
            ip: Some("1.2.3.4".to_string()),
            ipv4: None,
            ipv6: None,
            numwant: Some(50),
            key: Some("abc".to_string()),
            tracker_id: Some("id".to_string()),
        }
    }

    fn client_with_http(http: MockHttpClient) -> TrackerClient<MockHttpClient> {
        let config = ClientConfig::get(ClientType::QBittorrent, None);
        TrackerClient { http, client_config: config }
    }

    fn mock_http(status: StatusCode, body: Vec<u8>) -> MockHttpClient {
        let mut mock = MockHttpClient::new();
        mock.expect_get().returning(move |_, _| {
            let body = body.clone();
            Box::pin(async move { Ok(HttpResponse { status, body }) })
        });
        mock
    }

    #[test]
    fn test_tracker_event_as_str() {
        assert_eq!(TrackerEvent::Started.as_str(), Some("started"));
        assert_eq!(TrackerEvent::Stopped.as_str(), Some("stopped"));
        assert_eq!(TrackerEvent::Completed.as_str(), Some("completed"));
        assert_eq!(TrackerEvent::None.as_str(), None);
    }

    #[test]
    fn test_build_announce_url_params() -> Result<()> {
        let client = client()?;
        let hash = hash();
        let req = req(hash);
        let url = client.build_announce_url("https://tracker.test/announce", &req);
        let expect = encode_hash(hash);

        assert!(url.contains(&format!("info_hash={expect}")));
        assert!(url.contains("peer_id=peerid12345678901234"));
        assert!(url.contains("port=6881"));
        assert!(url.contains("uploaded=123"));
        assert!(url.contains("downloaded=456"));
        assert!(url.contains("left=789"));
        assert!(url.contains("compact=1"));
        assert!(url.contains("no_peer_id=1"));
        assert!(url.contains("event=started"));
        assert!(url.contains("ip=1.2.3.4"));
        assert!(url.contains("numwant=50"));
        assert!(url.contains("key=abc"));
        assert!(url.contains("trackerid=id"));
        assert!(url.contains("supportcrypto=1"));
        Ok(())
    }

    #[test]
    fn test_build_announce_url_ipv4_ipv6_params() -> Result<()> {
        let client = client()?;
        let mut r = req(hash());
        r.ipv4 = Some("109.81.3.4".to_string());
        r.ipv6 = Some("2001:db8::7".to_string());
        let url = client.build_announce_url("https://tracker.test/announce", &r);
        assert!(url.contains("ipv4=109.81.3.4"));
        assert!(url.contains("ipv6=2001:db8::7"));
        Ok(())
    }

    #[test]
    fn test_is_public_ip_filters() {
        use std::net::IpAddr;
        for s in [
            "10.0.0.1",
            "192.168.1.1",
            "172.16.0.1",
            "127.0.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "203.0.113.7",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "ff02::1",
            "2001:db8::1",
        ] {
            let ip: IpAddr = s.parse().unwrap();
            assert!(!is_public_ip(&ip), "{s} must never be announced");
        }
        for s in ["8.8.8.8", "1.1.1.1", "2001:4860:4860::8888", "2a00:1028:8388:d996::d"] {
            let ip: IpAddr = s.parse().unwrap();
            assert!(is_public_ip(&ip), "{s} must be announcable");
        }
    }

    #[test]
    fn test_parse_probe_body_shapes() {
        // Exact live bodies observed from the helper services.
        assert_eq!(
            parse_probe_body(
                "IPv4,109.81.3.4,v1.1,,,See http://ip6.me/docs/ for api documentation",
                false
            ),
            Some("109.81.3.4".to_string())
        );
        assert_eq!(
            parse_probe_body(
                "IPv6,2a00:1028:8388:d996::d,v1.1,,,See http://ip6.me/docs/ for api documentation",
                true
            ),
            Some("2a00:1028:8388:d996::d".to_string())
        );
        // Plain IP with trailing newline also accepted.
        assert_eq!(parse_probe_body("109.81.3.4\n", false), Some("109.81.3.4".to_string()));
        // Wrong family, private address, and garbage all rejected.
        assert_eq!(parse_probe_body("IPv4,109.81.3.4,v1.1", true), None);
        assert_eq!(parse_probe_body("IPv4,10.0.0.1,v1.1", false), None);
        assert_eq!(parse_probe_body("nonsense", false), None);
    }

    #[test]
    fn test_egress_addrs_invalid_host() {
        // .invalid never resolves: both families omitted, no network touched.
        assert_eq!(egress_addrs_for_tracker("https://tracker.invalid/announce"), (None, None));
    }

    #[test]
    fn test_build_announce_url_query_separator() -> Result<()> {
        let client = client()?;
        let hash = hash();
        let req = req(hash);
        let url = client.build_announce_url("https://tracker.test/announce?foo=1", &req);
        let expect = encode_hash(hash);

        assert!(url.contains(&format!("?foo=1&info_hash={expect}")));
        Ok(())
    }

    #[test]
    fn test_build_scrape_url_query_separator() -> Result<()> {
        let client = client()?;
        let hash = hash();
        let url = client.build_scrape_url("https://tracker.test/announce?pass=1", &hash);
        let expect = encode_hash(hash);

        assert!(url.starts_with("https://tracker.test/scrape?pass=1&info_hash="));
        assert!(url.ends_with(&expect));
        Ok(())
    }

    #[test]
    fn test_parse_announce_response_ok() -> Result<()> {
        let client = client()?;
        let data = b"d8:completei5e10:incompletei3e8:intervali1800e12:min intervali900e10:tracker id6:abc12315:warning message7:be caree";
        let res = client.parse_announce_response(data)?;

        assert_eq!(res.interval, 1800);
        assert_eq!(res.min_interval, Some(900));
        assert_eq!(res.tracker_id.as_deref(), Some("abc123"));
        assert_eq!(res.complete, 5);
        assert_eq!(res.incomplete, 3);
        assert_eq!(res.warning.as_deref(), Some("be care"));
        Ok(())
    }

    #[test]
    fn test_parse_announce_response_compact_peers() -> Result<()> {
        let client = client()?;
        let data =
            b"d8:intervali1800e8:completei1e10:incompletei0e5:peers6:\x01\x02\x03\x04\x1a\xe1e";
        let res = client.parse_announce_response(data)?;
        assert_eq!(res.peers.len(), 1);
        assert_eq!(res.peers[0].to_string(), "1.2.3.4:6881");
        Ok(())
    }

    #[test]
    fn test_parse_announce_response_dict_peers() -> Result<()> {
        let client = client()?;
        let data =
            b"d8:intervali1800e8:completei1e10:incompletei0e5:peersld2:ip8:10.0.0.14:porti51413eeee";
        let res = client.parse_announce_response(data)?;
        assert_eq!(res.peers.len(), 1);
        assert_eq!(res.peers[0].to_string(), "10.0.0.1:51413");
        Ok(())
    }

    #[test]
    fn test_parse_announce_response_compact_peers6() -> Result<()> {
        let client = client()?;
        let data = b"d8:intervali1800e8:completei1e10:incompletei0e6:peers618: \x01\r\xb8\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x01\x1a\xe1e";
        let res = client.parse_announce_response(data)?;
        assert_eq!(res.peers.len(), 1);
        assert_eq!(res.peers[0].to_string(), "[2001:db8::1]:6881");
        Ok(())
    }

    #[test]
    fn test_parse_announce_response_no_peers() -> Result<()> {
        let client = client()?;
        let data = b"d8:intervali1800e8:completei1e10:incompletei0ee";
        let res = client.parse_announce_response(data)?;
        assert!(res.peers.is_empty());
        Ok(())
    }

    #[test]
    fn test_parse_announce_response_failure() -> Result<()> {
        let client = client()?;
        let data = b"d14:failure reason11:bad passkeye";
        let res = client.parse_announce_response(data);

        assert!(matches!(&res, Err(TrackerError::TrackerFailure(_))));
        if let Err(TrackerError::TrackerFailure(reason)) = res {
            assert_eq!(reason, "bad passkey");
        }
        Ok(())
    }

    #[test]
    fn test_parse_announce_response_invalid() -> Result<()> {
        let client = client()?;
        let res = client.parse_announce_response(b"i42e");

        assert!(matches!(res, Err(TrackerError::InvalidResponse(_))));
        Ok(())
    }

    #[test]
    fn test_parse_scrape_response_ok() -> Result<()> {
        let client = client()?;
        let hash = hash();

        let mut stats = HashMap::new();
        stats.insert(b"complete".to_vec(), Value::Int(10));
        stats.insert(b"incomplete".to_vec(), Value::Int(2));
        stats.insert(b"downloaded".to_vec(), Value::Int(7));
        stats.insert(b"name".to_vec(), Value::Bytes(b"test".to_vec()));

        let mut files = HashMap::new();
        files.insert(hash.to_vec(), Value::Dict(stats));

        let mut root = HashMap::new();
        root.insert(b"files".to_vec(), Value::Dict(files));

        let data = bencode::encode(&Value::Dict(root))?;
        let res = client.parse_scrape_response(&data, &hash)?;

        assert_eq!(res.complete, 10);
        assert_eq!(res.incomplete, 2);
        assert_eq!(res.downloaded, 7);
        assert_eq!(res.name.as_deref(), Some("test"));
        Ok(())
    }

    #[test]
    fn test_parse_scrape_response_missing_files() -> Result<()> {
        let client = client()?;
        let hash = hash();

        let data = bencode::encode(&Value::Dict(HashMap::new()))?;
        let res = client.parse_scrape_response(&data, &hash);

        assert!(matches!(res, Err(TrackerError::InvalidResponse(_))));
        Ok(())
    }

    #[test]
    fn test_parse_scrape_response_missing_torrent() -> Result<()> {
        let client = client()?;
        let hash = hash();
        let other = [9u8; 20];

        let mut stats = HashMap::new();
        stats.insert(b"complete".to_vec(), Value::Int(1));
        stats.insert(b"incomplete".to_vec(), Value::Int(1));
        stats.insert(b"downloaded".to_vec(), Value::Int(1));

        let mut files = HashMap::new();
        files.insert(other.to_vec(), Value::Dict(stats));

        let mut root = HashMap::new();
        root.insert(b"files".to_vec(), Value::Dict(files));

        let data = bencode::encode(&Value::Dict(root))?;
        let res = client.parse_scrape_response(&data, &hash);

        assert!(matches!(res, Err(TrackerError::InvalidResponse(_))));
        Ok(())
    }

    #[test]
    fn test_format_response_preview_html() -> Result<()> {
        let client = client()?;
        let data = b"<!DOCTYPE html><title>Denied</title>";
        let msg = client.format_response_preview(data);

        assert_eq!(msg, "Received HTML page with title: \"Denied\"");
        Ok(())
    }

    #[test]
    fn test_format_response_preview_gzip() -> Result<()> {
        let client = client()?;
        let data = [0x1f, 0x8b, 0x08, 0x00];
        let msg = client.format_response_preview(&data);

        assert_eq!(
            msg,
            "Received gzip-compressed response (tracker may require Accept-Encoding header)"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_announce_ok() -> Result<()> {
        let mut dict = HashMap::new();
        dict.insert(b"interval".to_vec(), Value::Int(1800));
        dict.insert(b"complete".to_vec(), Value::Int(5));
        dict.insert(b"incomplete".to_vec(), Value::Int(2));
        let body = bencode::encode(&Value::Dict(dict))?;

        let http = mock_http(StatusCode::OK, body);
        let client = client_with_http(http);
        let res = client.announce("https://tracker.test/announce", &req(hash())).await?;

        assert_eq!(res.interval, 1800);
        assert_eq!(res.complete, 5);
        assert_eq!(res.incomplete, 2);
        Ok(())
    }

    #[tokio::test]
    async fn test_announce_http_error_status() -> Result<()> {
        let http = mock_http(StatusCode::INTERNAL_SERVER_ERROR, Vec::new());
        let client = client_with_http(http);
        let res = client.announce("https://tracker.test/announce", &req(hash())).await;

        assert!(matches!(&res, Err(TrackerError::HttpError(_))));
        if let Err(TrackerError::HttpError(msg)) = res {
            assert!(msg.contains("500"));
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_announce_invalid_bencode() -> Result<()> {
        let http = mock_http(StatusCode::OK, b"i42e".to_vec());
        let client = client_with_http(http);
        let res = client.announce("https://tracker.test/announce", &req(hash())).await;

        assert!(matches!(res, Err(TrackerError::InvalidResponse(_))));
        Ok(())
    }

    #[tokio::test]
    async fn test_scrape_ok() -> Result<()> {
        let hash = hash();
        let mut stats = HashMap::new();
        stats.insert(b"complete".to_vec(), Value::Int(2));
        stats.insert(b"incomplete".to_vec(), Value::Int(1));
        stats.insert(b"downloaded".to_vec(), Value::Int(3));

        let mut files = HashMap::new();
        files.insert(hash.to_vec(), Value::Dict(stats));

        let mut root = HashMap::new();
        root.insert(b"files".to_vec(), Value::Dict(files));

        let body = bencode::encode(&Value::Dict(root))?;
        let http = mock_http(StatusCode::OK, body);
        let client = client_with_http(http);
        let res = client.scrape("https://tracker.test/announce", &hash).await?;

        assert_eq!(res.complete, 2);
        assert_eq!(res.incomplete, 1);
        assert_eq!(res.downloaded, 3);
        Ok(())
    }

    #[tokio::test]
    async fn test_scrape_http_error_status() -> Result<()> {
        let http = mock_http(StatusCode::FORBIDDEN, Vec::new());
        let client = client_with_http(http);
        let res = client.scrape("https://tracker.test/announce", &hash()).await;

        assert!(matches!(&res, Err(TrackerError::HttpError(_))));
        if let Err(TrackerError::HttpError(msg)) = res {
            assert!(msg.contains("403"));
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_scrape_invalid_bencode() -> Result<()> {
        let http = mock_http(StatusCode::OK, b"i42e".to_vec());
        let client = client_with_http(http);
        let res = client.scrape("https://tracker.test/announce", &hash()).await;

        assert!(matches!(res, Err(TrackerError::InvalidResponse(_))));
        Ok(())
    }
}

<script>
  import Checkbox from '$lib/components/ui/checkbox.svelte';
  import Input from '$lib/components/ui/input.svelte';
  import Label from '$lib/components/ui/label.svelte';
  import { Network } from '@lucide/svelte';

  let {
    enabled = $bindable(false),
    maxPeers = $bindable(8),
    resampleIntervalSecs = $bindable(120),
    epsilonMinPercent = $bindable(1.0),
    phaseMinOursPercent = $bindable(90.0),
    phaseMinSeedFraction = $bindable(0.7),
    disabled = false,
    onchange,
  } = $props();
</script>

<div>
  <div class="flex items-center gap-3 mb-3">
    <Checkbox
      id="swarm-pacing-enabled"
      checked={enabled}
      {disabled}
      onchange={checked => {
        enabled = checked;
        onchange?.({ swarmPacingEnabled: checked });
      }}
    />
    <Label for="swarm-pacing-enabled" class="cursor-pointer font-medium flex items-center gap-2">
      <Network size={16} class="text-muted-foreground" />
      Swarm-paced download
    </Label>
  </div>

  {#if enabled}
    <div class="bg-muted/50 rounded-lg border border-border overflow-hidden">
      <div class="grid grid-cols-2">
        <div class="p-3 border-r border-border">
          <Label
            for="swarmMaxPeers"
            class="text-xs text-muted-foreground mb-2 block"
            title="Peers dialed per sample cycle. More peers means better consensus at the cost of more connections."
            >Peers / cycle</Label
          >
          <Input
            id="swarmMaxPeers"
            type="number"
            bind:value={maxPeers}
            {disabled}
            min="1"
            max="50"
            step="1"
            class="w-20 h-8 text-center font-medium"
            placeholder="8"
            oninput={() => onchange?.({ swarmMaxPeers: maxPeers })}
          />
        </div>
        <div class="p-3">
          <Label
            for="swarmResampleSecs"
            class="text-xs text-muted-foreground mb-2 block"
            title="Seconds between swarm samples. Shorter means fresher consensus and more dials."
            >Resample every</Label
          >
          <div class="flex items-center gap-2">
            <Input
              id="swarmResampleSecs"
              type="number"
              bind:value={resampleIntervalSecs}
              {disabled}
              min="30"
              max="3600"
              step="1"
              class="w-20 h-8 text-center font-medium"
              placeholder="120"
              oninput={() => onchange?.({ swarmResampleIntervalSecs: resampleIntervalSecs })}
            />
            <span class="text-xs text-muted-foreground">secs</span>
          </div>
        </div>
      </div>

      <div class="grid grid-cols-3 border-t border-border">
        <div class="p-3 border-r border-border">
          <Label
            for="swarmEpsilon"
            class="text-xs text-muted-foreground mb-2 block"
            title="Hysteresis floor in percentage points: how far ahead of the swarm we ride before throttling."
            >Band floor %</Label
          >
          <Input
            id="swarmEpsilon"
            type="number"
            bind:value={epsilonMinPercent}
            {disabled}
            min="0"
            max="10"
            step="0.1"
            class="w-20 h-8 text-center font-medium"
            placeholder="1.0"
            oninput={() => onchange?.({ swarmEpsilonMinPercent: epsilonMinPercent })}
          />
        </div>
        <div class="p-3 border-r border-border">
          <Label
            for="swarmPhaseOurs"
            class="text-xs text-muted-foreground mb-2 block"
            title="Our completion % at which a mostly-seed swarm lets us finish at full speed."
            >Finish from %</Label
          >
          <Input
            id="swarmPhaseOurs"
            type="number"
            bind:value={phaseMinOursPercent}
            {disabled}
            min="0"
            max="100"
            step="1"
            class="w-20 h-8 text-center font-medium"
            placeholder="90"
            oninput={() => onchange?.({ swarmPhaseMinOursPercent: phaseMinOursPercent })}
          />
        </div>
        <div class="p-3">
          <Label
            for="swarmPhaseSeeds"
            class="text-xs text-muted-foreground mb-2 block"
            title="Tracker seed fraction at which the swarm counts as completing."
            >Seed fraction</Label
          >
          <Input
            id="swarmPhaseSeeds"
            type="number"
            bind:value={phaseMinSeedFraction}
            {disabled}
            min="0"
            max="1"
            step="0.05"
            class="w-20 h-8 text-center font-medium"
            placeholder="0.7"
            oninput={() => onchange?.({ swarmPhaseMinSeedFraction: phaseMinSeedFraction })}
          />
        </div>
      </div>

      <div
        class="px-4 py-2 bg-muted/50 border-t border-border text-xs text-muted-foreground text-center"
      >
        Download follows the swarm consensus and pauses when ahead. New samples every
        {resampleIntervalSecs ?? 120}s from up to {maxPeers ?? 8} peers.
        <!-- ?? fallbacks are display hints mirroring backend defaults; unset values are omitted from payloads -->
      </div>
    </div>
  {/if}
</div>

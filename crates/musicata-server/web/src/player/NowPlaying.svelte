<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { player } from "../lib/player.svelte";
  import { dsp } from "../lib/dsp.svelte";
  import { audioDevices } from "../lib/audioDevices.svelte";
  import { meter } from "../lib/meter.svelte";
  import { statsPanel } from "../lib/statsPanel.svelte";
  import { startRadio } from "../lib/playback";
  import { sendCommand, type PlayerCommand } from "../lib/commands";
  import type { RepeatMode } from "../types/RepeatMode";
  import SeekBar from "./SeekBar.svelte";

  let { onclose, hidden = false }: { onclose: () => void; hidden?: boolean } = $props();
  import QueueDrawer from "./QueueDrawer.svelte";

  const REPEAT_NEXT: Record<RepeatMode, RepeatMode> = { off: "all", all: "one", one: "off" };

  function send(command: PlayerCommand) {
    sendCommand(player.target, command);
  }

  // Now-playing text reads `player.nowPlaying` (i.e. `player.playback`), which a progress
  // tick never changes — so these nodes are untouched on the per-second hot path.
  const subtitle = $derived(
    player.nowPlaying
      ? [player.nowPlaying.artist, player.nowPlaying.album].filter(Boolean).join(" · ")
      : "Select a track to begin.",
  );
</script>
<section class="now-playing-panel" aria-label="Now Playing" {hidden}>
  <header class="detail-panel-head">
    <h2>Now Playing</h2>
    <div class="playback-modes" role="group" aria-label="Playback options">
      <button class="ghost-button" type="button" aria-label="Shuffle" title="Shuffle" aria-pressed={player.shuffle} onclick={() => send({command:"set_shuffle",enabled:!player.shuffle})}>
        <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M3 6h3c5 0 7 12 12 12h3M18 15l3 3-3 3M3 18h3c2 0 4-3 6-6s4-6 6-6h3M18 3l3 3-3 3"/></svg>
      </button>
      <button class="ghost-button" type="button" aria-label={`Repeat: ${player.repeat}`} title={`Repeat: ${player.repeat}`} aria-pressed={player.repeat !== "off"} onclick={() => send({command:"set_repeat",mode:REPEAT_NEXT[player.repeat]})}>
        <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M4 11V8a3 3 0 0 1 3-3h13M17 2l3 3-3 3M20 13v3a3 3 0 0 1-3 3H4M7 16l-3 3 3 3"/>{#if player.repeat === "one"}<path d="M11 11l1-1v5"/>{/if}</svg>
      </button>
    </div>
    <button class="ghost-button close-now" type="button" aria-label="Close Now Playing" onclick={onclose}><svg viewBox="0 0 24 24" aria-hidden="true"><path d="m6 6 12 12M18 6 6 18"/></svg></button>
  </header>
  <div class="playing-detail">
    {#if player.nowPlaying?.artwork_url}<img class="playing-cover" src={player.nowPlaying.artwork_url} alt="" />{:else}<div class="playing-cover fallback" aria-hidden="true">♪</div>{/if}
    <div><h3>{player.nowPlaying?.title ?? "Nothing playing"}</h3><p>{subtitle}</p></div>
  </div>
  <QueueDrawer embedded />
  <section class="playing-tools" aria-label="Sound and playback tools"><h3>Sound &amp; playback tools</h3>
  <div class="playing-options">
    <SeekBar />
  <div class="transport-aux">
    {#if audioDevices.hasPresets && player.isBrowserOutput}
      <div class="output-switch" role="group" aria-label="Output">
        {#each audioDevices.presets as preset (preset.id)}
          <button
            type="button"
            class="output-btn"
            class:active={preset.id === audioDevices.activeId}
            aria-pressed={preset.id === audioDevices.activeId}
            title={`Play on ${preset.label}`}
            onclick={() => audioDevices.switchTo(preset.id)}
          >
            {preset.label}
          </button>
        {/each}
      </div>
    {/if}
    <input
      type="range"
      min="0"
      max="100"
      value={player.volume}
      aria-label="Volume"
      onchange={(e) => {
        const v = Number((e.currentTarget as HTMLInputElement).value);
        send({ command: "set_volume", volume: v });
        audioDevices.rememberVolume(v);
      }}
    />
    <button
      class="eq-btn radio-btn"
      type="button"
      title="Start radio from this track"
      disabled={!player.nowPlaying?.track_id}
      onclick={() => {
        const id = player.nowPlaying?.track_id;
        if (id) startRadio(id);
      }}
    >
      Radio
    </button>
    <button
      class="eq-btn"
      class:active={meter.open}
      type="button"
      title="VU meter"
      aria-pressed={meter.open}
      onclick={() => meter.toggle()}
    >
      VU
    </button>
    <button
      class="eq-btn"
      class:active={statsPanel.open}
      type="button"
      title="Listening stats"
      aria-pressed={statsPanel.open}
      onclick={() => statsPanel.toggle()}
    >
      Stats
    </button>
    <button
      class="eq-btn"
      class:active={dsp.enabled}
      type="button"
      title="Equalizer"
      aria-pressed={dsp.panelOpen}
      onclick={() => (dsp.panelOpen = !dsp.panelOpen)}
    >
      EQ
    </button>
  </div>
  </div>
  </section>
</section>
<style>
  .now-playing-panel { height:100%; overflow:auto; background:var(--panel); }
  .detail-panel-head { position:sticky; top:0; z-index:1; display:flex; align-items:center; justify-content:space-between; padding:10px 16px; background:var(--panel); border-bottom:1px solid var(--line); }
  h2 { margin:0; font-family:var(--font-display); font-size:1.25rem; }
  .playing-detail { display:flex; align-items:center; gap:16px; padding:16px; }
  .playing-cover { width:64px; height:64px; flex:none; object-fit:cover; border-radius:10px; }
  .fallback { display:grid; place-items:center; color:var(--accent); background:var(--panel-strong); font-size:2rem; }
  h3 { font-family:var(--font-display); font-size:1.15rem; margin:0 0 6px; overflow-wrap:anywhere; }
  p { font-size:.85rem; color:var(--muted); margin:0; overflow-wrap:anywhere; }
  .playing-options { padding:0 16px 16px; display:grid; gap:12px; }
  .playing-options :global(.seek-row) { display:flex; max-width:none; }
  .transport-aux { display:flex; flex-wrap:wrap; gap:8px; justify-content:flex-start; }
  .transport-aux > input { width:100%; }
  button { min-height:44px; }
  .detail-panel-head { gap:6px; }
  .detail-panel-head h2 { flex:1; }
  .playback-modes { display:flex; gap:2px; }
  .playback-modes button, .close-now { width:44px; padding:10px; flex:none; }
  .playback-modes button[aria-pressed="true"] { color:var(--accent); background:var(--accent-soft); }
  svg { display:block; width:22px; height:22px; fill:none; stroke:currentColor; stroke-width:1.7; stroke-linecap:round; stroke-linejoin:round; }
  .playing-tools { margin:16px; }
  .playing-tools h3 { padding:12px 0; color:var(--muted); font-family:var(--font-ui); font-size:.85rem; }
  .playing-options { padding:0 0 16px; }
  @media(max-width:400px) { .playing-detail { padding:10px 16px; gap:12px; } .playing-cover { width:48px; height:48px; } }
  @media(max-height:500px) { .playing-detail { display:none; } }

  .output-switch { display:flex; flex-wrap:wrap; gap:4px; width:100%; }
  .output-btn { padding:8px 12px; border:1px solid var(--line); background:var(--panel-strong); color:var(--text); border-radius:8px; }
  .output-btn.active { border-color:var(--accent); color:var(--accent); }
  .now-playing-panel :global(.queue-drawer.embedded) { position:static; inset:auto; }
  .now-playing-panel :global(.queue-list) { overflow:visible; }
  @media(max-height:500px) { .playing-cover { width:56px; height:56px; } .playing-detail { padding:8px 16px; } }
</style>

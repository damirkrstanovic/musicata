<script lang="ts">
  import { player } from "../lib/player.svelte";
  import { dsp } from "../lib/dsp.svelte";
  import { audioDevices } from "../lib/audioDevices.svelte";
  import { meter } from "../lib/meter.svelte";
  import { statsPanel } from "../lib/statsPanel.svelte";
  import { startRadio, togglePlayback, next, previous } from "../lib/playback";
  import { sendCommand, type PlayerCommand } from "../lib/commands";
  import type { RepeatMode } from "../types/RepeatMode";
  import SeekBar from "./SeekBar.svelte";

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

<footer class="transport" data-status={player.status}>
  <div class="transport-now">
    <div class="now-art">
      {#if player.nowPlaying?.artwork_url}
        <img src={player.nowPlaying.artwork_url} alt="" />
      {:else}
        <span class="now-art-fallback">♪</span>
      {/if}
    </div>
    <div class="now-text">
      <strong id="now-title"
        >{player.nowPlaying?.title ??
          (player.connection === "online"
            ? "Nothing playing"
            : player.connection === "connecting"
              ? "Connecting…"
              : "Reconnecting…")}</strong
      >
      {#if player.connection === "online"}
        <span>{subtitle}</span>
      {:else}
        <span class="conn-sub">
          Lost the server connection.
          <button type="button" class="conn-reload" onclick={() => location.reload()}>Reload</button>
        </span>
      {/if}
    </div>
  </div>

  <SeekBar />

  <div class="transport-main">
    <div class="transport-buttons">
      <button
        class="toggle"
        class:active={player.shuffle}
        type="button"
        title="Shuffle"
        aria-pressed={player.shuffle}
        onclick={() => send({ command: "set_shuffle", enabled: !player.shuffle })}>⤨</button
      >
      <button class="control" type="button" title="Previous" onclick={() => previous()}>⏮</button>
      <button
        class="control play"
        class:needs-tap={player.playBlocked}
        type="button"
        title={player.status === "playing" && !player.playBlocked ? "Pause" : "Play"}
        onclick={() => togglePlayback()}
        >{player.status === "playing" && !player.playBlocked ? "❚❚" : "▶"}</button
      >
      <button class="control" type="button" title="Next" onclick={() => next()}>⏭</button>
      <button
        class="toggle"
        class:active={player.repeat !== "off"}
        type="button"
        title="Repeat"
        data-mode={player.repeat}
        onclick={() => send({ command: "set_repeat", mode: REPEAT_NEXT[player.repeat] })}>↻</button
      >
    </div>
  </div>

  <div class="transport-aux">
    {#if audioDevices.hasPresets}
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
      ((•))
    </button>
    <button
      class="eq-btn"
      class:active={meter.open}
      type="button"
      title="VU meter"
      aria-pressed={meter.open}
      onclick={() => meter.toggle()}
    >
      ▮▮
    </button>
    <button
      class="eq-btn"
      class:active={statsPanel.open}
      type="button"
      title="Listening stats"
      aria-pressed={statsPanel.open}
      onclick={() => statsPanel.toggle()}
    >
      ◔
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
    <button
      class="queue-btn"
      type="button"
      title="Queue"
      aria-pressed={player.queueOpen}
      onclick={() => (player.queueOpen = !player.queueOpen)}
    >
      <span class="queue-glyph">≣</span><span class="queue-count">{player.queue.length}</span>
    </button>
  </div>
</footer>

<style>
  .output-switch {
    display: inline-flex;
    gap: 2px;
    margin-right: 0.4rem;
    border: 1px solid rgba(255, 255, 255, 0.14);
    border-radius: 0.5rem;
    overflow: hidden;
  }
  .output-btn {
    background: transparent;
    border: none;
    color: inherit;
    padding: 0.3rem 0.55rem;
    font-size: 0.75rem;
    cursor: pointer;
    white-space: nowrap;
  }
  .output-btn.active {
    background: rgba(212, 175, 55, 0.18);
    color: #d4af37;
  }
  /* Autoplay blocked: the server thinks we're playing but the browser refused sound. Pulse the
     play control so the user knows a tap is needed to actually start it. */
  .control.play.needs-tap {
    color: #d4af37;
    animation: needs-tap-pulse 1.2s ease-in-out infinite;
  }
  @keyframes needs-tap-pulse {
    0%,
    100% {
      opacity: 1;
    }
    50% {
      opacity: 0.45;
    }
  }
  .conn-sub {
    color: #e0a86b;
  }
  .conn-reload {
    background: none;
    border: none;
    padding: 0;
    color: inherit;
    font: inherit;
    text-decoration: underline;
    cursor: pointer;
  }
</style>

<script lang="ts">
  import { player } from "../lib/player.svelte";
  import { dsp } from "../lib/dsp.svelte";
  import { meter } from "../lib/meter.svelte";
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
      <strong id="now-title">{player.nowPlaying?.title ?? "Nothing playing"}</strong>
      <span>{subtitle}</span>
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
      <button class="control" type="button" title="Previous" onclick={() => send({ command: "previous" })}>⏮</button>
      <button
        class="control play"
        type="button"
        title={player.status === "playing" ? "Pause" : "Play"}
        onclick={() => send({ command: player.status === "playing" ? "pause" : "play" })}
        >{player.status === "playing" ? "❚❚" : "▶"}</button
      >
      <button class="control" type="button" title="Next" onclick={() => send({ command: "next" })}>⏭</button>
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
    <input
      type="range"
      min="0"
      max="100"
      value={player.volume}
      aria-label="Volume"
      onchange={(e) =>
        send({ command: "set_volume", volume: Number((e.currentTarget as HTMLInputElement).value) })}
    />
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

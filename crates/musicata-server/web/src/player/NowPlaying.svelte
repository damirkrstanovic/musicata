<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { player } from "../lib/player.svelte";
  import { dsp } from "../lib/dsp.svelte";
  import { audioDevices } from "../lib/audioDevices.svelte";
  import { meter } from "../lib/meter.svelte";
  import { statsPanel } from "../lib/statsPanel.svelte";
  import { favorites } from "../lib/favorites.svelte";
  import { sizedArtwork, initial } from "../lib/dom";
  import { startRadio } from "../lib/playback";
  import { sendCommand, type PlayerCommand } from "../lib/commands";
  import type { RepeatMode } from "../types/RepeatMode";
  import SeekBar from "./SeekBar.svelte";
  import QueueDrawer from "./QueueDrawer.svelte";
  import PlaybackIcon from "./PlaybackIcon.svelte";

  let { onclose, hidden = false }: { onclose: () => void; hidden?: boolean } = $props();
  const REPEAT_NEXT: Record<RepeatMode, RepeatMode> = { off: "all", all: "one", one: "off" };
  const artwork = $derived(player.nowPlaying?.artwork_url ?? null);
  const artworkSrc = $derived(sizedArtwork(artwork, 600));
  let sampled = $state<{url: string; color: string} | null>(null);
  let failed = $state<string | null>(null);
  const background = $derived(sampled?.url === artwork && sampled ? sampled.color : "#182127");

  function send(command: PlayerCommand) { sendCommand(player.target, command); }

  // A bounded sample only when a new cover loads. Playback ticks never touch this path.
  function loaded(image: HTMLImageElement) {
    if (!image.isConnected || image.dataset.artwork !== artwork || !artwork) return;
    failed = null;
    try {
      const canvas = document.createElement("canvas");
      canvas.width = canvas.height = 16;
      const context = canvas.getContext("2d", {willReadFrequently: true});
      if (!context) return;
      context.drawImage(image, 0, 0, 16, 16);
      const pixels = context.getImageData(0, 0, 16, 16).data;
      const sum = [0, 0, 0];
      let weight = 0;
      for (let i = 0; i < pixels.length; i += 4) {
        const alpha = pixels[i + 3] / 255;
        weight += alpha;
        for (let channel = 0; channel < 3; channel++) sum[channel] += pixels[i + channel] * alpha;
      }
      if (!weight) return;
      const mean = sum.map(value => value / weight);
      const low = Math.min(...mean);
      // Keep every channel between 22 and 48 so text contrast is independent of artwork.
      sampled = {url: artwork, color: "rgb(" + mean.map(value => Math.round(22 + (value - low) * .1)).join(",") + ")"};
    } catch { /* Missing, undecodable or unreadable artwork keeps the neutral background. */ }
  }
</script>
<section class="now-playing-panel listening-room" aria-label="Now Playing" {hidden} style:--listening-bg={background}>
  <div class="playing-detail">
    <div class="playing-cover" class:unavailable={!artwork || failed === artwork}>
      <span class="cover-fallback" aria-hidden="true">{initial(player.nowPlaying?.album || player.nowPlaying?.title || "Musicata")}</span>
      {#if artworkSrc && failed !== artwork}
        {#key artwork}
          <img src={artworkSrc} data-artwork={artwork} alt="" onload={event => loaded(event.currentTarget as HTMLImageElement)}
            onerror={event => { if (event.currentTarget.isConnected && (event.currentTarget as HTMLImageElement).dataset.artwork === artwork) failed = artwork; }} />
        {/key}
      {/if}
    </div>
    <div class="playing-caption">
      <div class="playing-title-row">
        <h2 class="listening-title">{player.nowPlaying?.title || "Nothing playing"}</h2>
        {#if player.nowPlaying?.track_id}
          <button class="favorite-track" type="button" aria-label="Favorite track" aria-pressed={favorites.has(player.nowPlaying.track_id)}
            onclick={() => { const id = player.nowPlaying?.track_id; if (id) favorites.toggleTrack(id); }}>
            <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M20 5c-3-3-7-1-8 1-1-2-5-4-8-1s-1 7 8 14C21 12 23 8 20 5Z"/></svg>
          </button>
        {/if}
      </div>
      <p class="playing-artist">{player.nowPlaying?.artist || (player.nowPlaying ? "Unknown artist" : "Choose an album from your library.")}</p>
      {#if player.nowPlaying?.album}<p class="playing-album">{player.nowPlaying.album}</p>{/if}
      {#if player.nowPlaying && (!artwork || failed === artwork)}<p class="artwork-unavailable">Artwork unavailable</p>{/if}
    </div>
  </div>
  <div class="listening-queue">
    <QueueDrawer embedded listening>
      {#snippet controls()}
        <div class="playback-modes" role="group" aria-label="Playback options">
          <button type="button" aria-label="Shuffle" title="Shuffle" aria-pressed={player.shuffle}
            onclick={() => send({command:"set_shuffle",enabled:!player.shuffle})}><PlaybackIcon name="shuffle" /></button>
          <button type="button" aria-label={'Repeat: '+player.repeat} title={'Repeat: '+player.repeat} aria-pressed={player.repeat !== "off"}
            onclick={() => send({command:"set_repeat",mode:REPEAT_NEXT[player.repeat]})}><PlaybackIcon name="repeat" />{#if player.repeat === "one"}<span class="repeat-one">1</span>{/if}</button>
          <button class="close-now" type="button" aria-label="Close Now Playing" title="Close Now Playing" onclick={onclose}><svg viewBox="0 0 24 24" aria-hidden="true"><path d="m6 6 12 12M18 6 6 18"/></svg></button>
        </div>
      {/snippet}
    </QueueDrawer>
  </div>
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
  .now-playing-panel { height:100%; min-height:0; display:grid; grid-template-columns:minmax(0,1fr) minmax(340px,35%); grid-template-rows:minmax(0,1fr) auto; background:var(--listening-bg); overflow:hidden; }
  .now-playing-panel[hidden] { display:none; }
  .playing-detail { grid-column:1; grid-row:1; min-height:0; overflow:auto; display:flex; flex-direction:column; align-items:center; justify-content:safe center; padding:28px 36px 16px; gap:24px; }
  .playing-cover { position:relative; width:min(100%,440px,42vh); aspect-ratio:1; flex:none; background:var(--panel-strong); box-shadow:0 14px 35px #0004; overflow:hidden; }
  .playing-cover img { position:absolute; inset:0; display:block; width:100%; height:100%; object-fit:cover; }
  .cover-fallback { position:absolute; inset:0; display:grid; place-items:center; font-family:var(--font-display); font-size:clamp(3rem,10vw,7rem); color:var(--accent); }
  .playing-caption { width:min(100%,440px); }
  .playing-title-row { display:flex; align-items:flex-start; gap:16px; }
  .listening-title { flex:1; min-width:0; font-size:clamp(1.8rem,3vw,2.8rem); line-height:1.13; font-weight:400; font-family:var(--font-display); overflow-wrap:anywhere; }
  .playing-artist { margin:10px 0 0; color:var(--muted); font-size:1rem; overflow-wrap:anywhere; }
  .playing-album,.artwork-unavailable { margin:10px 0 0; color:var(--muted); font-size:.78rem; overflow-wrap:anywhere; }
  .favorite-track { width:44px; min-width:44px; height:44px; border:0; background:none; color:var(--muted); cursor:pointer; }
  .favorite-track[aria-pressed="true"] { color:var(--accent); }
  .favorite-track[aria-pressed="true"] svg { fill:var(--accent-soft); }
  .listening-queue { grid-column:2; grid-row:1 / 3; display:flex; min-height:0; border-left:1px solid var(--line); background:var(--panel); padding:24px 24px 0; }
  .playback-modes { display:flex; align-items:center; }
  .playback-modes button { position:relative; display:grid; place-items:center; height:44px; width:44px; padding:10px; background:none; border:0; color:var(--muted); cursor:pointer; border-radius:50%; }
  .playback-modes button:hover { color:var(--text); background:var(--panel-strong); }
  .playback-modes button[aria-pressed="true"] { color:var(--accent); }
  .repeat-one { position:absolute; top:15px; left:19px; font-size:9px; }
  svg { width:22px; height:22px; fill:none; stroke:currentColor; stroke-width:1.7; stroke-linecap:round; stroke-linejoin:round; }
  .playing-tools { grid-column:1; grid-row:2; padding:0 36px 20px; min-width:0; }
  .playing-tools h3 { max-width:440px; margin:0 auto 8px; font-size:.75rem; font-weight:400; color:var(--muted); }
  .playing-options { display:grid; gap:8px; max-width:440px; margin:auto; }
  .playing-options :global(.seek-row) { display:none; }
  .transport-aux { display:flex; flex-wrap:wrap; align-items:center; justify-content:center; gap:6px; }
  .transport-aux > input { flex:1; min-width:90px; max-width:180px; accent-color:var(--accent); }
  .transport-aux .eq-btn { min-height:40px; border:0; border-radius:4px; background:none; padding:8px 10px; }
  .output-switch { display:flex; flex-wrap:wrap; gap:4px; width:100%; justify-content:center; }
  .output-btn { min-height:40px; padding:8px 12px; border:1px solid var(--line); background:transparent; color:var(--text); border-radius:6px; }
  .output-btn.active { border-color:var(--accent); color:var(--accent); }
  .now-playing-panel :global(.queue-drawer.embedded) { position:static; inset:auto; width:100%; min-height:0; background:transparent; animation:none; }
  .now-playing-panel :global(.queue-head) { flex:none; padding:0 0 16px; border:0; gap:6px; }
  .now-playing-panel :global(.queue-head > strong) { font-family:var(--font-display); font-size:1.8rem; font-weight:400; }
  .now-playing-panel :global(.queue-list) { flex:1; min-height:0; overflow:auto; padding:12px 0; border-top:1px solid var(--line); }
  .now-playing-panel :global(.queue-row) { padding:10px 8px; gap:10px; border:0; min-height:68px; border-radius:5px; }
  .now-playing-panel :global(.queue-row.current) { background:var(--accent-soft); }
  .now-playing-panel :global(.q-art) { display:none; }
  .now-playing-panel :global(.q-title) { font-family:var(--font-ui); font-size:.9rem; font-weight:400; }
  .now-playing-panel :global(.q-sub) { font-size:.75rem; }
  .now-playing-panel :global(.queue-options) { padding:8px 0; }
  .now-playing-panel :global(.queue-empty) { flex:1; }
  .now-playing-panel :global(.queue-footer) { flex:none; border-top:1px solid var(--line); display:flex; justify-content:flex-end; padding:8px 0; }
  .now-playing-panel :global(.queue-footer .ghost-button) { border:0; background:none; color:var(--muted); font-size:.78rem; }
  @media(max-width:1023px) {
    .now-playing-panel { display:flex; flex-direction:column; }
    .playing-detail { flex:none; flex-direction:row; justify-content:flex-start; padding:16px 20px; gap:18px; overflow:hidden; }
    .playing-cover { width:96px; }
    .playing-caption { min-width:0; width:auto; }
    .listening-title { font-size:1.65rem; display:-webkit-box; line-clamp:2; -webkit-line-clamp:2; -webkit-box-orient:vertical; overflow:hidden; }
    .playing-artist,.playing-album { white-space:nowrap; text-overflow:ellipsis; overflow:hidden; }
    .playing-artist { font-size:.85rem; margin-top:6px; }
    .playing-album,.artwork-unavailable { font-size:.7rem; margin-top:6px; }
    .favorite-track { display:none; }
    .listening-queue { flex:1; min-height:120px; border:0; padding:4px 16px 0; }
    .now-playing-panel :global(.queue-head) { padding:0 0 6px; }
    .now-playing-panel :global(.queue-head > strong) { font-size:1.4rem; }
    .now-playing-panel :global(.queue-list) { padding:4px 0; }
    .now-playing-panel :global(.queue-row) { min-height:58px; padding:5px 4px; }
    .now-playing-panel :global(.queue-footer) { padding:0; }
    .playing-tools { flex:none; padding:4px 16px 8px; background:var(--panel); }
    .playing-tools h3 { display:none; }
    .playing-options { max-width:none; }
    .transport-aux { justify-content:flex-start; }
    .transport-aux > input { flex:1 1 60px; min-width:60px; max-width:100px; }
    .transport-aux .eq-btn { min-height:36px; padding:6px 8px; }
    .output-switch { width:100%; justify-content:flex-start; }
    .output-btn { min-height:36px; padding:6px 8px; }
  }
  @media(max-width:400px) { .playing-cover { width:72px; } .playing-detail { padding:12px 16px; gap:12px; } .listening-title { font-size:1.4rem; } }
  @media(max-width:359px) { .playing-cover { width:56px; } .playing-detail { padding-block:8px; } .playing-album { display:none; } }
  @media(max-height:500px) and (min-width:600px) {
    .now-playing-panel { display:grid; grid-template-columns:minmax(0,1fr) minmax(300px,40%); grid-template-rows:minmax(0,1fr) auto; }
    .playing-detail { grid-column:1; grid-row:1; padding:12px 20px; gap:16px; }
    .playing-cover { width:100px; }
    .listening-title { font-size:1.5rem; }
    .playing-tools { grid-column:1; grid-row:2; padding:4px 16px; background:transparent; }
    .listening-queue { grid-column:2; grid-row:1/3; min-height:0; padding:4px 16px 0; }
    .now-playing-panel :global(.queue-row) { min-height:52px; }
    .now-playing-panel :global(.queue-footer .ghost-button) { min-height:36px; }
    .transport-aux > input { max-width:110px; min-width:60px; }
  }
</style>

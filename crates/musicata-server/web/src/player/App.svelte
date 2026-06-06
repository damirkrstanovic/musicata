<script lang="ts">
  import { onMount, onDestroy } from "svelte";
  import { api } from "../lib/api";
  import { player } from "../lib/player.svelte";
  import { connectPlayer, type PlayerSocket, type ProgressTick } from "../lib/playerWs";
  import { BrowserAudio } from "../lib/audio";
  import { sendCommand } from "../lib/commands";
  import { setMediaMetadata, setMediaPosition, setMediaHandlers } from "../lib/media";
  import type { PlaybackState } from "../types/PlaybackState";
  import Footer from "./Footer.svelte";

  let audioEl: HTMLAudioElement;
  let audio: BrowserAudio | null = null;
  let ws: PlayerSocket | null = null;
  let starting = $state(false);

  // Hot path: a tick moves only elapsed/duration (+ the OS scrubber).
  function applyTick(tick: ProgressTick) {
    player.elapsed = tick.elapsed_seconds ?? 0;
    if (tick.duration_seconds != null) player.duration = tick.duration_seconds;
    setMediaPosition(player.elapsed, player.duration);
  }

  // Full snapshot: replace playback, reconcile the <audio> element, refresh OS metadata only
  // when the track or status actually changed.
  function applyState(next: PlaybackState) {
    const trackChanged =
      (player.playback?.now_playing?.track_id ?? null) !== (next.now_playing?.track_id ?? null);
    const statusChanged = player.playback?.status !== next.status;
    player.playback = next;
    if (!player.seekDragging) {
      player.elapsed = next.elapsed_seconds ?? 0;
      player.duration = next.duration_seconds ?? 0;
    }
    audio?.drive(next);
    if (trackChanged || statusChanged) setMediaMetadata(next.now_playing, next.status);
    setMediaPosition(player.elapsed, player.duration);
  }

  onMount(async () => {
    audio = new BrowserAudio(audioEl);
    audio.onProgress((msg) => ws?.send(msg));
    audio.onEnded(() => ws?.send({ type: "ended" }));
    audio.start();

    setMediaHandlers({
      play: () => sendCommand(player.activeId, { command: "play" }),
      pause: () => sendCommand(player.activeId, { command: "pause" }),
      previoustrack: () => sendCommand(player.activeId, { command: "previous" }),
      nexttrack: () => sendCommand(player.activeId, { command: "next" }),
      seekto: (d) => {
        if (d.seekTime != null)
          sendCommand(player.activeId, { command: "seek", position_seconds: d.seekTime });
      },
    });

    const players = await api.players();
    const browser = players.find((p) => p.kind === "browser") ?? players[0];
    if (!browser) return;
    player.activeId = browser.id;
    ws = connectPlayer(browser.id, { onState: applyState, onProgress: applyTick });
  });

  onDestroy(() => {
    ws?.close();
    audio?.stop();
  });

  // Temporary Phase-3a affordance: claim audio output and play the first album, so the
  // playback hot path actually runs. Replaced by real library views in the next sub-phase.
  async function playSomething() {
    if (!player.activeId) return;
    starting = true;
    try {
      const album = (await api.albums({ limit: 1 })).items[0];
      if (!album) return;
      const { tracks } = await api.albumTracks(album.id);
      if (!tracks.length) return;
      audio?.claim();
      audio?.primePlay(tracks[0].stream_url); // in-gesture, satisfies autoplay
      await sendCommand(player.activeId, {
        command: "play_tracks",
        track_ids: tracks.map((t) => t.id),
        start_index: 0,
      });
    } finally {
      starting = false;
    }
  }
</script>

<div class="player-shell">
  <header class="app-bar">
    <h1>Musicata</h1>
    <div class="app-bar-actions">
      <button class="primary-button" disabled={starting} onclick={playSomething}>Play an album</button>
      <a class="ghost-button" href="/v2/admin">Admin</a>
    </div>
  </header>
  <p class="admin-hint" style="padding: 0 1rem">
    Player shell (Phase 3a) — footer + playback hot path. Library views land next.
  </p>

  <Footer />
  <audio bind:this={audioEl} preload="none" hidden></audio>
</div>

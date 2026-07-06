// Bridges the views to the audio output: a view hands over a track list + start index, and
// we claim output, prime play() in the click gesture (autoplay), and issue the command.
import type { BrowserAudio } from "./audio";
import { api, type TrackRow } from "./api";
import { sendCommand } from "./commands";
import { player } from "./player.svelte";
import { radioMix } from "./radioMix.svelte";
import { nav } from "./nav.svelte";

let audio: BrowserAudio | null = null;

export function setAudio(instance: BrowserAudio): void {
  audio = instance;
}

/** The active browser audio driver (for the VU meter to read output levels). */
export function getAudio(): BrowserAudio | null {
  return audio;
}

/** Play `tracks` on the active player, starting at `startIndex`. Call from a click handler. */
export async function playTracks(tracks: TrackRow[], startIndex = 0): Promise<void> {
  if (!player.target || !tracks.length) return;
  // Only prime local audio when this tab is the output (browser target); MPD/zone targets
  // play on their own device.
  if (player.isBrowserOutput) {
    const start = tracks[startIndex] ?? tracks[0];
    audio?.claim();
    audio?.primePlay(start.stream_url);
  }
  await sendCommand(player.target, {
    command: "play_tracks",
    track_ids: tracks.map((t) => t.id),
    start_index: startIndex,
  });
}

// ---- Transport verbs ----
// EVERY playback-affecting command goes through these so no caller can forget the "claim +
// prime the browser output inside the gesture" ritual. Sending a raw `play`/`next`/… command
// straight to the browser output leaves `drive()` a no-op (the tab never claimed) or blocked by
// autoplay policy — the UI shows "playing" over silence. A source guard (tests/ui/
// transport-guard.mjs) enforces that these command literals live only in this file.

/** Pause the active target. */
export function pause(): void {
  if (player.target) void sendCommand(player.target, { command: "pause" });
}

/** Resume the active target. When this tab is the browser output, claim + prime the now-playing
 *  stream *inside the click gesture* (like {@link playTracks}) so the restored track actually
 *  plays: a bare `play` would arrive back over the WS *after* the gesture, and `drive()` would
 *  skip it because the tab never claimed output (autoplay blocks a gesture-less `el.play()`). */
export async function resume(): Promise<void> {
  if (!player.target) return;
  if (player.isBrowserOutput) {
    audio?.claim();
    const stream = player.nowPlaying?.stream_url;
    if (stream) audio?.primePlay(stream);
  }
  await sendCommand(player.target, { command: "play" });
}

/** Footer Play/Pause. */
export function togglePlayback(): void {
  // Blocked by autoplay: the server status already reads "playing", so a tap must force a real,
  // in-gesture play (resume) to clear it rather than toggling to pause.
  if (player.playBlocked) {
    void resume();
    return;
  }
  if (player.status === "playing") pause();
  else void resume();
}

/** Skip to the next / previous queued track. Claims the browser output inside the gesture so
 *  `drive()` plays the new track once the server's state arrives (the next track's stream URL
 *  isn't known until then, so there's nothing to prime yet — the claim is what matters). */
export function next(): void {
  step("next");
}
export function previous(): void {
  step("previous");
}
function step(command: "next" | "previous"): void {
  if (!player.target) return;
  if (player.isBrowserOutput) audio?.claim();
  void sendCommand(player.target, { command });
}

/** Start a "radio" from a seed track: the seed plus similar tracks. Call from a click handler. */
export async function startRadio(seedTrackId: string): Promise<void> {
  // Claim output inside the gesture before the await, so autoplay policy lets it play.
  if (player.isBrowserOutput) audio?.claim();
  const res = await api.trackRadio(seedTrackId, 25);
  await openMix(res?.tracks ?? [], seedTrackId);
}

/**
 * Start an audio "radio" from a seed track: a diverse station of tracks that *sound like* the
 * seed (musicata-ml embedding, artist-interleaved). Empty when the seed hasn't been analyzed —
 * no-op then, just like {@link startRadio}. Call from a click handler.
 */
export async function startAudioRadio(seedTrackId: string): Promise<void> {
  if (player.isBrowserOutput) audio?.claim();
  const res = await api.trackAudioRadio(seedTrackId, 25);
  await openMix(res?.tracks ?? [], seedTrackId);
}

/** Show the generated mix in the Mix view and play it. If the seed is already playing, keep it
 *  going and just queue the rest (no restart); otherwise play the whole mix from the seed. */
async function openMix(tracks: TrackRow[], seedTrackId: string): Promise<void> {
  if (!tracks.length) return;
  radioMix.set(tracks);
  nav.push({ name: "mix" });
  if (player.nowPlaying?.track_id === seedTrackId && player.status !== "stopped") {
    const rest = tracks.map((track) => track.id).filter((id) => id !== seedTrackId);
    if (rest.length) await sendCommand(player.target, { command: "enqueue", track_ids: rest });
    return;
  }
  await playTracks(tracks, 0);
}

/** Play an internet-radio stream on the active target. Call from a click handler. */
export async function playStream(url: string, title: string): Promise<void> {
  if (!player.target) return;
  if (player.isBrowserOutput) {
    audio?.claim();
    audio?.primePlay(url);
  }
  await sendCommand(player.target, { command: "play_stream", url, title });
}

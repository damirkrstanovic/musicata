// Bridges the views to the audio output: a view hands over a track list + start index, and
// we claim output, prime play() in the click gesture (autoplay), and issue the command.
import type { BrowserAudio } from "./audio";
import { api, type TrackRow } from "./api";
import { sendCommand } from "./commands";
import { player } from "./player.svelte";

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

/** Play a list of library track ids on the active target. Call from a click handler. */
export async function playTrackIds(ids: string[], startIndex = 0): Promise<void> {
  if (!player.target || !ids.length) return;
  if (player.isBrowserOutput) {
    const startId = ids[startIndex] ?? ids[0];
    audio?.claim();
    audio?.primePlay(`/api/tracks/${encodeURIComponent(startId)}/stream`);
  }
  await sendCommand(player.target, { command: "play_tracks", track_ids: ids, start_index: startIndex });
}

/** Start a "radio" from a seed track: the seed plus similar tracks. Call from a click handler. */
export async function startRadio(seedTrackId: string): Promise<void> {
  // Claim output inside the gesture before the await, so autoplay policy lets it play.
  if (player.isBrowserOutput) audio?.claim();
  const res = await api.trackRadio(seedTrackId, 25);
  const ids = res?.track_ids ?? [];
  if (!ids.length) return;
  // If the seed is the track already playing, keep it going and just queue the station after
  // it — no restart, no interruption. (The radio list includes the seed at index 0.)
  if (player.nowPlaying?.track_id === seedTrackId && player.status !== "stopped") {
    const rest = ids.filter((id) => id !== seedTrackId);
    if (rest.length) await sendCommand(player.target, { command: "enqueue", track_ids: rest });
    return;
  }
  await playTrackIds(ids);
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

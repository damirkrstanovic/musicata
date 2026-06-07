// Bridges the views to the audio output: a view hands over a track list + start index, and
// we claim output, prime play() in the click gesture (autoplay), and issue the command.
import type { BrowserAudio } from "./audio";
import type { TrackRow } from "./api";
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

/** Play an internet-radio stream on the active target. Call from a click handler. */
export async function playStream(url: string, title: string): Promise<void> {
  if (!player.target) return;
  if (player.isBrowserOutput) {
    audio?.claim();
    audio?.primePlay(url);
  }
  await sendCommand(player.target, { command: "play_stream", url, title });
}

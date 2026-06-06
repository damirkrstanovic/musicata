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

/** Play `tracks` on the active player, starting at `startIndex`. Call from a click handler. */
export async function playTracks(tracks: TrackRow[], startIndex = 0): Promise<void> {
  if (!player.activeId || !tracks.length) return;
  const start = tracks[startIndex] ?? tracks[0];
  audio?.claim();
  audio?.primePlay(start.stream_url);
  await sendCommand(player.activeId, {
    command: "play_tracks",
    track_ids: tracks.map((t) => t.id),
    start_index: startIndex,
  });
}

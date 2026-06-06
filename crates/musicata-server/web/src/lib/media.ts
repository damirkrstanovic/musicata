// navigator.mediaSession glue: OS now-playing metadata, the lock-screen scrubber, and the
// hardware/notification transport buttons (which dispatch PlayerCommands).
import type { PlaybackState } from "../types/PlaybackState";
import type { QueueItem } from "../types/QueueItem";

export function setMediaMetadata(now: QueueItem | null, status: PlaybackState["status"]): void {
  if (!("mediaSession" in navigator)) return;
  const ms = navigator.mediaSession;
  ms.playbackState = status === "playing" ? "playing" : status === "paused" ? "paused" : "none";
  ms.metadata = now
    ? new MediaMetadata({
        title: now.title || "Unknown",
        artist: now.artist || "",
        album: now.album || "",
        artwork: now.artwork_url ? [{ src: now.artwork_url, sizes: "512x512" }] : [],
      })
    : null;
}

export function setMediaPosition(elapsed: number, duration: number): void {
  if (!("mediaSession" in navigator)) return;
  try {
    if (duration > 0 && elapsed >= 0 && elapsed <= duration) {
      navigator.mediaSession.setPositionState({ duration, position: elapsed, playbackRate: 1 });
    } else {
      navigator.mediaSession.setPositionState();
    }
  } catch {
    // ignore invalid position state
  }
}

export type MediaAction =
  | "play"
  | "pause"
  | "previoustrack"
  | "nexttrack"
  | "stop"
  | "seekto"
  | "seekbackward"
  | "seekforward";

export function setMediaHandlers(
  handlers: Partial<Record<MediaAction, (details: MediaSessionActionDetails) => void>>,
): void {
  if (!("mediaSession" in navigator)) return;
  for (const [action, handler] of Object.entries(handlers)) {
    try {
      navigator.mediaSession.setActionHandler(action as MediaSessionAction, handler ?? null);
    } catch {
      // browser doesn't support this action
    }
  }
}

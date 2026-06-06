// Central player state (Svelte 5 runes). The hot path lives here: `elapsed`/`duration` are
// their OWN signals, mutated alone on every ~1/s progress tick, so the footer's time/seek
// re-render WITHOUT touching now-playing, queue, or any track-list highlight (those derive
// from `playback`, which a tick never changes). This replaces the old manual memoization.
import type { PlaybackState } from "../types/PlaybackState";
import type { QueueItem } from "../types/QueueItem";
import type { PlaybackStatus } from "../types/PlaybackStatus";
import type { RepeatMode } from "../types/RepeatMode";

class Player {
  activeId = $state<string | null>(null);
  playback = $state<PlaybackState | null>(null);

  // Hot-path signals — see the note above.
  elapsed = $state(0);
  duration = $state(0);
  seekDragging = $state(false);
  queueOpen = $state(false);

  get status(): PlaybackStatus {
    return this.playback?.status ?? "stopped";
  }
  get nowPlaying(): QueueItem | null {
    return this.playback?.now_playing ?? null;
  }
  get shuffle(): boolean {
    return this.playback?.shuffle ?? false;
  }
  get repeat(): RepeatMode {
    return this.playback?.repeat ?? "off";
  }
  get volume(): number {
    return this.playback?.volume ?? 100;
  }
  get queue(): QueueItem[] {
    return this.playback?.queue ?? [];
  }
  get queuePosition(): number {
    return this.playback?.queue_position ?? -1;
  }
}

export const player = new Player();

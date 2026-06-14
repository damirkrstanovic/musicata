// Central player state (Svelte 5 runes). The hot path lives here: `elapsed`/`duration` are
// their OWN signals, mutated alone on every ~1/s progress tick, so the footer's time/seek
// re-render WITHOUT touching now-playing, queue, or any track-list highlight (those derive
// from `playback`, which a tick never changes). This replaces the old manual memoization.
import type { PlaybackState } from "../types/PlaybackState";
import type { QueueItem } from "../types/QueueItem";
import type { PlaybackStatus } from "../types/PlaybackStatus";
import type { RepeatMode } from "../types/RepeatMode";

export type TargetKind = "player" | "zone";
export interface Target {
  kind: TargetKind;
  id: string;
}

class Player {
  activeId = $state<string | null>(null);
  activeKind = $state<TargetKind>("player");
  /** The browser player's id, and the zone it belongs to (if any) — the targets this tab
   *  outputs audio for. */
  browserId = $state<string | null>(null);
  browserZoneId = $state<string | null>(null);
  playback = $state<PlaybackState | null>(null);
  /** Live link to the active player's state stream. Drives the reconnecting indicator so a
   *  dropped/uninitialised connection reads as "Reconnecting…" instead of a misleading
   *  "Nothing playing". */
  connection = $state<"connecting" | "online" | "reconnecting">("connecting");

  // Hot-path signals — see the note above.
  elapsed = $state(0);
  duration = $state(0);
  seekDragging = $state(false);
  queueOpen = $state(false);

  get target(): Target | null {
    return this.activeId ? { kind: this.activeKind, id: this.activeId } : null;
  }
  /** True when this tab is the audio output: the active target is the browser player, or a
   *  zone that contains it. */
  get isBrowserOutput(): boolean {
    if (this.activeKind === "player") return this.activeId === this.browserId;
    return this.activeId !== null && this.activeId === this.browserZoneId;
  }

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

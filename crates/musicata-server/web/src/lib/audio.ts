// Drives the browser <audio> element from playback state and reports its position back to
// the server. The server then broadcasts ~1/s `type:"progress"` ticks to all sockets, which
// feed the hot path. Only one tab outputs at a time (claimed via localStorage).
import type { PlaybackState } from "../types/PlaybackState";

export interface ProgressReport {
  type: "progress";
  elapsed_seconds: number;
  duration_seconds: number | null;
}

export class BrowserAudio {
  private el: HTMLAudioElement;
  private claimed = false;
  private onReport?: (msg: ProgressReport) => void;
  private onEnd?: () => void;
  private timer?: ReturnType<typeof setInterval>;

  constructor(el: HTMLAudioElement) {
    this.el = el;
    el.addEventListener("ended", () => this.onEnd?.());
    el.addEventListener("loadedmetadata", () => this.report());
  }

  /** Mark this tab as the audio output. Must be called from a user gesture so play() works. */
  claim(): void {
    this.claimed = true;
    try {
      localStorage.setItem("musicata-output", "1");
    } catch {
      // private mode — fine
    }
  }
  get isClaimed(): boolean {
    return this.claimed;
  }

  onProgress(cb: (msg: ProgressReport) => void): void {
    this.onReport = cb;
  }
  onEnded(cb: () => void): void {
    this.onEnd = cb;
  }

  start(): void {
    this.timer = setInterval(() => {
      if (this.claimed && !this.el.paused) this.report();
    }, 1000);
  }
  stop(): void {
    if (this.timer) clearInterval(this.timer);
  }

  setVolume(volume: number): void {
    this.el.volume = Math.min(1, Math.max(0, volume / 100));
  }

  /** Start a stream right now. Call inside a user gesture so the browser's autoplay policy
   *  lets it play; `drive()` then keeps it in sync once the server's state arrives. */
  primePlay(streamUrl: string): void {
    if (!this.el.src.endsWith(streamUrl)) this.el.src = streamUrl;
    this.el.play().catch(() => {
      // gesture not honored / will retry via drive()
    });
  }

  private report(): void {
    const duration = Number.isFinite(this.el.duration) ? this.el.duration : null;
    this.onReport?.({
      type: "progress",
      elapsed_seconds: this.el.currentTime,
      duration_seconds: duration,
    });
  }

  /** Reconcile the <audio> element with the desired playback state. */
  drive(playback: PlaybackState): void {
    if (!this.claimed) return;
    if (playback.volume != null) this.setVolume(playback.volume);
    const now = playback.now_playing;
    if (playback.status === "playing" && now?.stream_url) {
      if (!this.el.src.endsWith(now.stream_url)) this.el.src = now.stream_url;
      // Apply an external seek (a >2s jump), ignoring echoes of our own reported position.
      const elapsed = playback.elapsed_seconds ?? 0;
      if (Math.abs(this.el.currentTime - elapsed) > 2) this.el.currentTime = elapsed;
      this.el.play().catch(() => {
        // autoplay policy: needs a user gesture
      });
    } else {
      this.el.pause();
    }
  }
}

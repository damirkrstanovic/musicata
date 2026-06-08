// Drives the browser <audio> element from playback state and reports its position back to
// the server. The server then broadcasts ~1/s `type:"progress"` ticks to all sockets, which
// feed the hot path. Only one tab outputs at a time (claimed via localStorage).
import type { PlaybackState } from "../types/PlaybackState";
import type { EqProfile } from "./dsp";

// Volume-leveling target (LUFS) and the true-peak ceiling (dBTP) the combined gain must not
// exceed. −14 LUFS is the streaming/Roon convention.
const LEVELING_TARGET_LUFS = -14;
const TRUE_PEAK_CEILING_DBTP = -1;

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

  // Web Audio graph. Built once during a play gesture (claim()) so the AudioContext starts
  // running, then all browser audio flows through it:
  //   source -> preamp -> [EQ biquads] -> destination
  //                                    \-> splitter -> analyserL/R   (taps for the VU meter)
  // EQ changes update the biquad chain live; the analysers feed `levels()` for the meter.
  private ctx?: AudioContext;
  private srcNode?: MediaElementAudioSourceNode;
  private preamp?: GainNode;
  private eqBands: BiquadFilterNode[] = [];
  private splitter?: ChannelSplitterNode;
  private analyserL?: AnalyserNode;
  private analyserR?: AnalyserNode;
  private bufL?: Float32Array<ArrayBuffer>;
  private bufR?: Float32Array<ArrayBuffer>;
  private profile: EqProfile | null = null;
  private graphFailed = false;

  // Volume leveling (EBU R128). `levelingGain` sits at the end of the chain; its gain is set
  // per track from the now-playing item's LUFS, combined with the EQ preamp for clip safety.
  private levelingGain?: GainNode;
  private eqPreampDb = 0;
  private levelingEnabled = false;
  private trackLufs: number | null = null;
  private trackTruePeak: number | null = null;

  constructor(el: HTMLAudioElement) {
    this.el = el;
    el.addEventListener("ended", () => this.onEnd?.());
    el.addEventListener("loadedmetadata", () => this.report());
    // Web Audio unlock: build + resume the graph on the first user gesture *anywhere*, so EQ
    // and metering engage no matter how playback is first triggered (footer Play, a track row,
    // media keys) and regardless of the browser's autoplay policy. createMediaElementSource can
    // only ever run inside a gesture once; doing it here guarantees that.
    const unlock = () => {
      this.ensureGraph();
      this.ctx?.resume().catch(() => {});
      if (this.ctx?.state === "running" || this.graphFailed) {
        for (const ev of ["pointerdown", "keydown", "click"] as const) {
          document.removeEventListener(ev, unlock, true);
        }
      }
    };
    for (const ev of ["pointerdown", "keydown", "click"] as const) {
      document.addEventListener(ev, unlock, true);
    }
  }

  /** Mark this tab as the audio output. Must be called from a user gesture so play() works. */
  claim(): void {
    this.claimed = true;
    try {
      localStorage.setItem("musicata-output", "1");
    } catch {
      // private mode — fine
    }
    this.ensureGraph(); // build the graph in this gesture so the context starts running
    this.ctx?.resume().catch(() => {});
  }
  get isClaimed(): boolean {
    return this.claimed;
  }

  /** Set (or clear, with `null`) the active EQ profile and apply it live. */
  setEq(profile: EqProfile | null): void {
    this.profile = profile;
    this.ensureGraph();
    if (this.ctx) {
      this.rebuildChain();
      this.ctx.resume().catch(() => {});
    }
  }

  /** Instantaneous RMS level (0..~1, linear) per channel, or null if metering is unavailable. */
  levels(): { l: number; r: number } | null {
    if (!this.analyserL || !this.analyserR || !this.bufL || !this.bufR) return null;
    this.analyserL.getFloatTimeDomainData(this.bufL);
    this.analyserR.getFloatTimeDomainData(this.bufR);
    return { l: rms(this.bufL), r: rms(this.bufR) };
  }

  private ensureGraph(): void {
    if (this.ctx || this.graphFailed) return;
    const Ctor =
      window.AudioContext ??
      (window as unknown as { webkitAudioContext?: typeof AudioContext }).webkitAudioContext;
    if (!Ctor) {
      this.graphFailed = true;
      return;
    }
    try {
      const ctx = new Ctor();
      this.ctx = ctx;
      this.srcNode = ctx.createMediaElementSource(this.el);
      this.preamp = ctx.createGain();
      this.splitter = ctx.createChannelSplitter(2);
      this.analyserL = ctx.createAnalyser();
      this.analyserR = ctx.createAnalyser();
      this.analyserL.fftSize = 1024;
      this.analyserR.fftSize = 1024;
      this.analyserL.smoothingTimeConstant = 0;
      this.analyserR.smoothingTimeConstant = 0;
      this.bufL = new Float32Array(new ArrayBuffer(this.analyserL.fftSize * 4));
      this.bufR = new Float32Array(new ArrayBuffer(this.analyserR.fftSize * 4));
      this.levelingGain = ctx.createGain();
      this.rebuildChain();
    } catch {
      this.graphFailed = true; // already tapped, or Web Audio unavailable
      // If the element was already tapped before the failure, route it straight to output so
      // playback isn't lost (we just go without EQ/metering).
      try {
        this.srcNode?.connect(this.ctx!.destination);
      } catch {
        // nothing tapped — native path is intact
      }
    }
  }

  /** (Re)build source -> preamp -> [bands] -> destination, with a post-EQ stereo tap to the
   *  analysers. Safe to call on a running context; used for both initial build and EQ edits. */
  private rebuildChain(): void {
    const { ctx, srcNode: src, preamp, levelingGain: lvl, splitter, analyserL: aL, analyserR: aR } =
      this;
    if (!ctx || !src || !preamp || !lvl || !splitter || !aL || !aR) return;
    for (const n of [src, preamp, lvl, ...this.eqBands]) {
      try {
        n.disconnect();
      } catch {
        // not connected yet
      }
    }
    this.eqBands = [];
    const p = this.profile;
    this.eqPreampDb = p ? p.preampDb : 0;
    preamp.gain.value = p ? 10 ** (p.preampDb / 20) : 1;
    src.connect(preamp);
    let prev: AudioNode = preamp;
    if (p) {
      for (const band of p.bands) {
        const f = ctx.createBiquadFilter();
        f.type = band.type;
        f.frequency.value = band.freq;
        f.Q.value = band.q;
        f.gain.value = band.gain;
        prev.connect(f);
        prev = f;
        this.eqBands.push(f);
      }
    }
    // Leveling is the last stage; the analyser tap is post-leveling so the VU shows output.
    prev.connect(lvl);
    lvl.connect(ctx.destination);
    lvl.connect(splitter);
    splitter.connect(aL, 0);
    splitter.connect(aR, 1);
    this.applyLeveling();
  }

  /** Enable/disable per-track volume leveling (the apply side; analysis is server-side). */
  setLeveling(enabled: boolean): void {
    this.levelingEnabled = enabled;
    this.applyLeveling();
  }

  /** The now-playing track's measured loudness (LUFS) + true-peak (dBTP), or nulls if unknown. */
  setTrackLoudness(lufs: number | null, truePeak: number | null): void {
    this.trackLufs = lufs;
    this.trackTruePeak = truePeak;
    this.applyLeveling();
  }

  /** Compute the leveling gain (target − track LUFS), then attenuate it so the *combined* gain
   *  with the EQ preamp keeps true-peak ≤ the ceiling — the two gains sum, so they must be
   *  clip-checked together or they double-clip. */
  private applyLeveling(): void {
    if (!this.levelingGain) return;
    let db = 0;
    if (this.levelingEnabled && this.trackLufs != null && Number.isFinite(this.trackLufs)) {
      db = LEVELING_TARGET_LUFS - this.trackLufs;
      if (this.trackTruePeak != null) {
        const maxLeveling = TRUE_PEAK_CEILING_DBTP - this.trackTruePeak - this.eqPreampDb;
        if (db > maxLeveling) db = maxLeveling;
      }
    }
    this.levelingGain.gain.value = 10 ** (db / 20);
  }

  onProgress(cb: (msg: ProgressReport) => void): void {
    this.onReport = cb;
  }
  onEnded(cb: () => void): void {
    this.onEnd = cb;
  }

  start(): void {
    this.timer = setInterval(() => {
      if (this.claimed && !this.el.paused) {
        this.report();
        // Safety net: re-resume the EQ graph if a gesture-less enable left it suspended.
        if (this.ctx?.state === "suspended") this.ctx.resume().catch(() => {});
      }
    }, 1000);
  }
  stop(): void {
    if (this.timer) clearInterval(this.timer);
  }

  /** Pause output immediately. Used when the server connection drops so already-buffered audio
   *  doesn't keep playing on after the server is gone. */
  pause(): void {
    this.el.pause();
  }

  setVolume(volume: number): void {
    this.el.volume = Math.min(1, Math.max(0, volume / 100));
  }

  /** Start a stream right now. Call inside a user gesture so the browser's autoplay policy
   *  lets it play; `drive()` then keeps it in sync once the server's state arrives. */
  primePlay(streamUrl: string): void {
    this.ctx?.resume().catch(() => {});
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
      this.ctx?.resume().catch(() => {});
      this.el.play().catch(() => {
        // autoplay policy: needs a user gesture
      });
    } else {
      this.el.pause();
    }
  }
}

function rms(buf: Float32Array): number {
  let sum = 0;
  for (let i = 0; i < buf.length; i++) sum += buf[i] * buf[i];
  return Math.sqrt(sum / buf.length);
}

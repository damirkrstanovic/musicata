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
  private bypassed = false;
  private graphFailed = false;

  // Set by the app when this tab is the active browser output. On the first user gesture we then
  // adopt output (claimed = true) so ANY transport control drives audio — not only the paths that
  // call claim() explicitly. (No cross-tab output arbitration yet, so two tabs both targeting the
  // browser player could both adopt; single-tab is the common case.)
  private designatedOutput = false;
  // True while the desired state is "playing" but the element's play() was blocked (autoplay
  // policy). Surfaced so the UI can offer "tap to resume" instead of a silent, false "playing".
  private blocked = false;
  private onBlockedCb?: (blocked: boolean) => void;

  // Room correction (Phase 4): a ConvolverNode after the EQ bands loaded from a profile's WAV
  // impulse response. `convBuffer` is the decoded IR; `convProfileId` tracks which profile's IR
  // is loaded so we don't refetch on every rebuild.
  private convBuffer: AudioBuffer | null = null;
  private convProfileId: string | null = null;
  private conv?: ConvolverNode;

  // The output device chosen before the graph exists; applied once `ensureGraph` builds it.
  // `undefined` = never set (leave the OS default untouched).
  private pendingSinkId: string | null | undefined = undefined;

  // Volume leveling (EBU R128). `levelingGain` sits at the end of the chain; its gain is set
  // per track from the now-playing item's LUFS, combined with the EQ preamp for clip safety.
  private levelingGain?: GainNode;
  private eqPreampDb = 0;
  private levelingMode: "off" | "track" | "album" = "off";
  private trackLufs: number | null = null;
  private trackTruePeak: number | null = null;
  // The now-playing track's *album* aggregate loudness. Preferred over the per-track values
  // when present ("album mode"), so an album's internal dynamics survive normalization.
  private albumLufs: number | null = null;
  private albumTruePeak: number | null = null;

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
      // Adopt output on the first gesture if this tab is the active browser output, so a restored
      // track plays via any control (footer, media keys, next/prev) — not only the paths that
      // claim() explicitly. drive() then acts on the next state instead of silently no-opping.
      if (this.designatedOutput) this.claimed = true;
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

  /** Tell the driver whether this tab is the active browser output. When true, the first user
   *  gesture adopts output (see the constructor's `unlock`), so any transport control drives
   *  audio without an explicit claim. */
  setDesignatedOutput(on: boolean): void {
    this.designatedOutput = on;
  }

  /** Observe the "playback blocked" state: true when we want to play but the browser refused
   *  `el.play()` (autoplay policy), false once it plays. The UI uses this to offer a tap-to-resume
   *  affordance instead of showing a silent, false "playing". */
  onBlocked(cb: (blocked: boolean) => void): void {
    this.onBlockedCb = cb;
  }
  private setBlocked(v: boolean): void {
    if (this.blocked === v) return;
    this.blocked = v;
    this.onBlockedCb?.(v);
  }

  /** Set (or clear, with `null`) the active EQ profile and apply it live. Stores the profile and
   *  applies it only if the audio graph already exists — the graph (and its `AudioContext`) is
   *  built on the first user gesture, not here, so restoring a saved profile at load doesn't
   *  create a suspended context (which browsers warn about: "AudioContext was prevented from
   *  starting"). `ensureGraph()` applies the stored profile + IR when it builds. */
  setEq(profile: EqProfile | null): void {
    this.profile = profile;
    if (this.ctx) {
      this.rebuildChain();
      this.ctx.resume().catch(() => {});
      void this.loadRoomIr(profile); // async; rebuilds again once the IR is decoded
    }
  }

  /** Fetch + decode a profile's room-correction impulse response (or clear it), then rebuild so
   *  the ConvolverNode is inserted. No-ops if the same profile's IR is already loaded. */
  private async loadRoomIr(profile: EqProfile | null): Promise<void> {
    const id = profile?.roomIr ? profile.id : null;
    if (id === this.convProfileId) return;
    // Without a context there's nothing to load into yet; don't record `convProfileId` so the
    // IR is (re)loaded once `ensureGraph()` builds the graph on the first gesture.
    if (!this.ctx) return;
    this.convProfileId = id;
    if (!id) {
      this.convBuffer = null;
      this.rebuildChain();
      return;
    }
    let decoded: AudioBuffer | null = null;
    try {
      const resp = await fetch(`/api/dsp/profiles/${encodeURIComponent(id)}/impulse`);
      if (!resp.ok) throw new Error("no impulse");
      decoded = await this.ctx.decodeAudioData(await resp.arrayBuffer());
    } catch {
      decoded = null; // missing/undecodable → just skip convolution, never silence
    }
    // Only commit if this is still the active request — a newer profile switch (a later
    // loadRoomIr) must not be clobbered by this one's late-resolving fetch.
    if (this.convProfileId !== id) return;
    this.convBuffer = decoded;
    this.rebuildChain();
  }

  /** A/B bypass: route source straight to output (no preamp/EQ), keeping the active profile. */
  setBypass(on: boolean): void {
    this.bypassed = on;
    if (this.ctx) this.rebuildChain();
  }

  /** Route output to a specific device via `AudioContext.setSinkId`. Unsupported on
   *  Safari/Firefox — there we no-op and the OS default is used (the profile + volume still
   *  swap, which covers the common single-output setup). `null`/"" selects the default. Stores
   *  the choice and applies it only once the graph exists (built on the first gesture), so
   *  restoring a saved device at load doesn't create a suspended `AudioContext`. */
  async setSink(sinkId: string | null): Promise<void> {
    this.pendingSinkId = sinkId;
    if (this.ctx) await this.applySink();
  }

  /** Apply the stored output-device choice to the existing context. */
  private async applySink(): Promise<void> {
    const ctx = this.ctx as (AudioContext & { setSinkId?: (id: string) => Promise<void> }) | undefined;
    if (!ctx || typeof ctx.setSinkId !== "function" || this.pendingSinkId === undefined) return;
    try {
      await ctx.setSinkId(this.pendingSinkId ?? "");
    } catch {
      // invalid / unplugged device — fall back to the OS default rather than dropping audio
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
      // Apply anything chosen before the graph existed (restored EQ IR + output device).
      void this.loadRoomIr(this.profile);
      void this.applySink();
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
    const previous: AudioNode[] = [src, preamp, lvl, ...this.eqBands];
    if (this.conv) previous.push(this.conv); // disconnect the old convolver too, else it leaks
    for (const n of previous) {
      try {
        n.disconnect();
      } catch {
        // not connected yet
      }
    }
    this.eqBands = [];
    this.conv = undefined;
    const p = this.bypassed ? null : this.profile;
    this.eqPreampDb = p ? p.preampDb : 0;
    let prev: AudioNode;
    if (this.bypassed) {
      // Straight passthrough for A/B (no preamp, no bands).
      prev = src;
    } else {
      preamp.gain.value = p ? 10 ** (p.preampDb / 20) : 1;
      src.connect(preamp);
      prev = preamp;
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
      // Room correction: a ConvolverNode after the EQ, when the profile carries an IR.
      if (this.convBuffer) {
        const conv = ctx.createConvolver();
        conv.normalize = false; // correction IRs must NOT be loudness-normalised
        conv.buffer = this.convBuffer;
        this.conv = conv; // keep a handle so the next rebuild disconnects it
        prev.connect(conv);
        prev = conv;
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

  /** Set the volume-leveling mode (the apply side; analysis is server-side): `off`, `track`
   *  (per-track loudness), or `album` (the album aggregate, falling back to per-track). */
  setLevelingMode(mode: "off" | "track" | "album"): void {
    this.levelingMode = mode;
    this.applyLeveling();
  }

  /** The now-playing track's measured loudness (LUFS) + true-peak (dBTP), and its album
   *  aggregate when known. Nulls mean unknown. */
  setTrackLoudness(
    lufs: number | null,
    truePeak: number | null,
    albumLufs: number | null = null,
    albumTruePeak: number | null = null,
  ): void {
    this.trackLufs = lufs;
    this.trackTruePeak = truePeak;
    this.albumLufs = albumLufs;
    this.albumTruePeak = albumTruePeak;
    this.applyLeveling();
  }

  /** Compute the leveling gain (target − loudness), then attenuate it so the *combined* gain
   *  with the EQ preamp keeps true-peak ≤ the ceiling — the two gains sum, so they must be
   *  clip-checked together or they double-clip. Prefers the album aggregate when available
   *  (album mode) so tracks within an album keep their relative loudness, falling back to the
   *  per-track measurement. */
  private applyLeveling(): void {
    if (!this.levelingGain) return;
    // Album mode prefers the album aggregate and falls back to per-track; track mode always
    // uses the per-track measurement; off applies no gain.
    let lufs: number | null = null;
    let truePeak: number | null = null;
    if (this.levelingMode === "album") {
      lufs = this.albumLufs ?? this.trackLufs;
      truePeak = this.albumLufs != null ? this.albumTruePeak : this.trackTruePeak;
    } else if (this.levelingMode === "track") {
      lufs = this.trackLufs;
      truePeak = this.trackTruePeak;
    }
    let db = 0;
    if (lufs != null && Number.isFinite(lufs)) {
      db = LEVELING_TARGET_LUFS - lufs;
      if (truePeak != null) {
        const maxLeveling = TRUE_PEAK_CEILING_DBTP - truePeak - this.eqPreampDb;
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
    this.el
      .play()
      .then(() => this.setBlocked(false))
      .catch(() => {
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
      // Track whether the browser actually let us play: a rejection (autoplay policy) surfaces a
      // tap-to-resume affordance rather than a silent, false "playing".
      this.el
        .play()
        .then(() => this.setBlocked(false))
        .catch(() => this.setBlocked(true));
    } else {
      this.el.pause();
      this.setBlocked(false);
    }
  }
}

function rms(buf: Float32Array): number {
  let sum = 0;
  for (let i = 0; i < buf.length; i++) sum += buf[i] * buf[i];
  return Math.sqrt(sum / buf.length);
}

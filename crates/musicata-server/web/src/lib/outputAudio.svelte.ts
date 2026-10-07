// SPDX-License-Identifier: AGPL-3.0-or-later
import type { OutputDspState } from "../types/OutputDspState";
import type { StereoLevels } from "../types/StereoLevels";
import type { EqProfile } from "./dsp";
import type { BrowserAudio } from "./audio";
import { reportDiagnostic,setDiagnosticRendererToken } from "./diagnostics";

export interface AudioConfig { state: OutputDspState; profile: EqProfile | null; correction_profile: EqProfile | null; listening_profile: EqProfile | null; meter_subscribed: boolean }
interface AudioFrame {
  type: string;
  reporter_token?:string;
  config?: AudioConfig;
  session_id?: string;
  revision?: number;
  sequence?: number;
  levels?: StereoLevels | null;
}

function connect(id: string, receive: (frame: AudioFrame) => void, opened?: () => void, disconnected?: () => void) {
  let socket: WebSocket | null = null;
  let closed = false;
  let retry: ReturnType<typeof setTimeout> | undefined;
  function open() {
    if (closed) return;
    const scheme = location.protocol === "https:" ? "wss" : "ws";
    const current = new WebSocket(`${scheme}://${location.host}/api/players/${encodeURIComponent(id)}/audio/ws`);
    socket = current;
    current.onopen = () => { if (!closed) opened?.(); };
    current.onmessage = event => {
      if (closed || socket !== current) return;
      try { receive(JSON.parse(event.data) as AudioFrame); } catch { /* Ignore malformed frames. */ }
    };
    current.onclose = () => {
      if (closed || socket !== current) return;
      disconnected?.();
      retry = setTimeout(open, 2000);
    };
    current.onerror = () => current.close();
  }
  open();
  return {
    send(frame: unknown) { if (socket?.readyState === WebSocket.OPEN) socket.send(JSON.stringify(frame)); },
    close() { closed = true; clearTimeout(retry); socket?.close(); },
  };
}

class OutputAudio {
  state = $state<OutputDspState | null>(null);
  levels = $state<StereoLevels | null>(null);
  name = $state("");
  private channel: ReturnType<typeof connect> | null = null;
  private subscribed = false;
  private lastSample = 0;
  private expiry: ReturnType<typeof setInterval> | undefined;

  select(id: string, name: string, onConfig: (config: AudioConfig) => void) {
    this.close();
    this.name = name;
    let sequence = 0;
    let session = "";
    this.channel = connect(id, frame => {
      if (frame.config) {
        if (frame.config.state.output_id !== id) return;
        if (session !== frame.config.state.session_id) { sequence = 0; session = frame.config.state.session_id; this.levels = null; }
        this.state = frame.config.state;
        onConfig(frame.config);
      } else if (frame.type === "levels" && this.state && frame.session_id === session && frame.revision === this.state.desired_revision) {
        if ((frame.sequence ?? 0) <= sequence) return;
        sequence = frame.sequence!;
        this.lastSample = performance.now();
        this.levels = frame.levels ?? null;
      }
    }, () => this.setSubscribed(this.subscribed), () => { this.levels = null; this.state = null; });
    this.expiry = setInterval(() => { if (performance.now() - this.lastSample > 1000) this.levels = null; }, 100);
  }

  setSubscribed(enabled: boolean) {
    this.subscribed = enabled;
    this.channel?.send({ type: "meter_subscription", enabled });
    if (!enabled) this.levels = null;
  }

  close() {
    this.channel?.close();
    this.channel = null;
    clearInterval(this.expiry);
    this.state = null;
    this.levels = null;
  }
}

export const outputAudio = new OutputAudio();

/** Own audio-channel configuration and reporting separately from the selected controller target. */
export function connectBrowserRenderer(id: string, audio: BrowserAudio) {
  audio.setRendererAllowed(false);
  let config: AudioConfig | null = null;
  let granted = false;
  let requested = false;
  let sequence = 0;
  let appliedRevision = -1;
  let lastRequest = 0;
  const channel = connect(id, frame => {
    if (frame.type === "renderer_denied") { setDiagnosticRendererToken(null); granted = false; requested = false; audio.setRendererAllowed(false); }
    if (frame.type === "renderer_granted") { setDiagnosticRendererToken(frame.reporter_token??null); granted = true; audio.setRendererAllowed(true); }
    if (frame.config) {
      config = frame.config;
      if (granted && appliedRevision !== config.state.desired_revision) {
        const profile = config.state.selection.enabled ? config.profile : null;
        const applying = config;
        appliedRevision = applying.state.desired_revision;
        sequence = 0;
        void audio.applyOutputEq(profile).then(() => {
          if (!granted || config?.state.session_id !== applying.state.session_id || config.state.desired_revision !== applying.state.desired_revision) return;
          reportDiagnostic("browser.dsp", false);
          channel.send({ type: "dsp_applied", session_id: applying.state.session_id, revision: applying.state.desired_revision });
        }).catch(error => {
          if (!granted || config?.state.session_id !== applying.state.session_id || config.state.desired_revision !== applying.state.desired_revision) return;
          reportDiagnostic("browser.dsp", true);
          channel.send({ type: "dsp_applied", session_id: applying.state.session_id, revision: applying.state.desired_revision, error: String(error) });
        });
      }
    }
  }, () => { requested = false; granted = false; appliedRevision = -1; }, () => { setDiagnosticRendererToken(null); granted = false; requested = false; audio.setRendererAllowed(false); });
  const timer = setInterval(() => {
    if (!audio.isClaimed) return;
    if (!requested && performance.now() - lastRequest > 2000) {
      lastRequest = performance.now(); requested = true;
      channel.send({ type: "renderer" });
    }
    if (!granted || !config?.meter_subscribed || appliedRevision < 0) return;
    const levels = audio.levels();
    if (levels) channel.send({ type: "levels", session_id: config.state.session_id, revision: appliedRevision,
      sequence: ++sequence, levels: { rms_l: levels.l, rms_r: levels.r, peak_l: levels.peakL, peak_r: levels.peakR } });
  }, 100);
  return { close() { setDiagnosticRendererToken(null); clearInterval(timer); channel.close(); } };
}

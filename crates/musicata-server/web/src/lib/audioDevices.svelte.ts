// SPDX-License-Identifier: AGPL-3.0-or-later
// Output presets + the speakers/headphones switcher (the "home-office" case). An OutputPreset
// binds a physical sink + a server DSP profile + a remembered volume. These live per-browser in
// localStorage because output device IDs are browser/machine-scoped and not portable (the DSP
// profiles they reference ARE server-synced — see dsp.svelte.ts). Applying a preset is wired in
// player/App.svelte: swap the EQ profile + the AudioContext sink + the remembered volume.
import { dsp } from "./dsp.svelte";
import { getAudio } from "./playback";
import { sendCommand } from "./commands";
import { player } from "./player.svelte";

export interface OutputPreset {
  id: string;
  label: string;
  /** AudioContext.setSinkId target; null = OS default device. */
  sinkId: string | null;
  /** Which server DspProfile to apply; null = no correction. */
  profileId: string | null;
  /** Remembered volume (0–100) — per output, a safety feature (headphones ≫ speaker level). */
  volume: number;
}

const LS_KEY = "musicata-outputs";

interface Persisted {
  presets: OutputPreset[];
  activeId: string | null;
}

function randomId(): string {
  return `out-${Math.random().toString(36).slice(2, 9)}`;
}

class AudioDevices {
  presets = $state<OutputPreset[]>([]);
  activeId = $state<string | null>(null);
  /** Enumerated audio-output devices (labels may be blank without mic permission). */
  devices = $state<MediaDeviceInfo[]>([]);

  constructor() {
    try {
      const raw = localStorage.getItem(LS_KEY);
      if (raw) {
        const p = JSON.parse(raw) as Persisted;
        this.presets = Array.isArray(p.presets) ? p.presets : [];
        this.activeId = p.activeId ?? null;
      }
    } catch {
      // private mode / corrupt — start empty
    }
  }

  get active(): OutputPreset | null {
    return this.presets.find((p) => p.id === this.activeId) ?? null;
  }
  get hasPresets(): boolean {
    return this.presets.length > 0;
  }

  async refreshDevices(): Promise<void> {
    if (!navigator.mediaDevices?.enumerateDevices) return;
    try {
      const all = await navigator.mediaDevices.enumerateDevices();
      this.devices = all.filter((d) => d.kind === "audiooutput");
    } catch {
      this.devices = [];
    }
  }

  init(): void {
    this.refreshDevices();
    navigator.mediaDevices?.addEventListener?.("devicechange", () => this.refreshDevices());
  }

  addPreset(label: string): OutputPreset {
    const preset: OutputPreset = { id: randomId(), label, sinkId: null, profileId: null, volume: 60 };
    this.presets = [...this.presets, preset];
    this.save();
    return preset;
  }
  updatePreset(id: string, patch: Partial<Omit<OutputPreset, "id">>): void {
    this.presets = this.presets.map((p) => (p.id === id ? { ...p, ...patch } : p));
    this.save();
  }
  removePreset(id: string): void {
    this.presets = this.presets.filter((p) => p.id !== id);
    if (this.activeId === id) this.activeId = null;
    this.save();
  }
  setActive(id: string | null): void {
    this.activeId = id;
    this.save();
  }

  /** Apply the active preset: swap the EQ profile + the output sink (+ remembered volume on an
   *  explicit switch). `withVolume` is false on startup so we don't override the restored
   *  playback level just from booting. */
  applyActive(withVolume: boolean): void {
    const preset = this.active;
    if (!preset) return;
    if (preset.profileId) dsp.setActive(preset.profileId);
    else dsp.setEnabled(false);
    void getAudio()?.setSink(preset.sinkId);
    if (withVolume) sendCommand(player.target, { command: "set_volume", volume: preset.volume });
  }

  /** Switch to a preset and apply it (profile + sink + remembered volume). */
  switchTo(id: string): void {
    this.setActive(id);
    this.applyActive(true);
  }
  /** Remember the current volume on the active preset (called when the user moves the slider). */
  rememberVolume(volume: number): void {
    if (this.activeId) this.updatePreset(this.activeId, { volume });
  }

  private save(): void {
    try {
      localStorage.setItem(
        LS_KEY,
        JSON.stringify({ presets: this.presets, activeId: this.activeId } satisfies Persisted),
      );
    } catch {
      // private mode — fine
    }
  }
}

export const audioDevices = new AudioDevices();

// SPDX-License-Identifier: AGPL-3.0-or-later
// EQ state (Svelte 5 runes). The **profile library** (`custom`) is now stored **server-side**
// (so presets follow the user across devices — see crate::dsp), loaded via `load()`. The
// Selection and bypass belong to each server output. Volume leveling and physical browser
// sink bindings remain local. The owning renderer applies the audio-channel configuration.
import { parseParametricEq, BUILT_IN_PROFILES, type EqProfile } from "./dsp";
import { api } from "./api";
import { outputAudio, type AudioConfig } from "./outputAudio.svelte";
import type { OutputDspSelection } from "../types/OutputDspSelection";

const LS_KEY = "musicata-dsp"; // client prefs (+ legacy `custom`, migrated to the server once)

/** Volume-leveling mode: off, per-track, or per-album (EBU R128 normalization).
 *  Album uses the track's album aggregate (falling back to per-track), so an album's
 *  internal dynamics survive normalization. */
export type LevelingMode = "off" | "track" | "album";

function isLevelingMode(value: unknown): value is LevelingMode {
  return value === "off" || value === "track" || value === "album";
}

class Dsp {
  enabled = $state(false);
  activeId = $state<string | null>(null);
  /** The server-stored profile library (built-ins live in `dsp.ts` and are merged in). */
  custom = $state<EqProfile[]>([]);
  panelOpen = $state(false);
  /** Volume leveling (EBU R128 normalization across tracks / albums). */
  levelingMode = $state<LevelingMode>("off");
  loaded = $state(false);
  outputId = $state<string | null>(null);
  error = $state<string | null>(null);
  private writes = Promise.resolve();
  private legacySelection: OutputDspSelection | null = null;
  private migrating = false;

  bindOutput(id: string, name: string): void {
    this.outputId = id;
    this.enabled = false;
    this.activeId = null;
    this.error = null;
    outputAudio.select(id, name, config => this.adopt(config));
  }

  private adopt(config: AudioConfig): void {
    if (config.state.output_id !== this.outputId) return;
    if (!config.state.configured && config.state.measurement_point === "browser_output") void this.migrateLegacy();
    this.enabled = config.state.selection.enabled;
    this.activeId = config.state.selection.profile_id;
    if (config.profile) this.custom = [...this.custom.filter(p => p.id !== config.profile!.id), config.profile];
  }

  private async migrateLegacy(): Promise<void> {
    if (!this.loaded || this.migrating || !this.legacySelection || !this.outputId) return;
    const id = this.outputId;
    const selection = this.legacySelection;
    const profile = this.profiles.find(p => p.id === selection.profile_id);
    if (!profile) return;
    this.migrating = true;
    try {
      if (!this.isCustom(profile.id)) await api.saveDspProfile(profile);
      await api.saveOutputDsp(id, selection, true);
      this.legacySelection = null;
      this.saveLocal();
    } catch { /* Preserve legacy preference for the next load. */ }
    finally {this.migrating = false;}
  }

  private write(id = this.outputId, selection: OutputDspSelection = {profile_id: this.activeId, enabled: this.enabled}): void {
    if (!id) return;
    const profile = this.profiles.find(p => p.id === selection.profile_id);
    this.writes = this.writes.then(async () => {
      if (profile && !this.isCustom(profile.id)) {
        const existing = (await api.dspProfiles()).find(p => p.id === profile.id);
        if (!existing) await api.saveDspProfile(profile);
        this.custom = [...this.custom.filter(p => p.id !== profile.id), existing ?? profile];
      }
      await api.saveOutputDsp(id, selection);
    }).catch(error => { if (id === this.outputId) this.error = String(error); });
  }
  constructor() {
    // Client preferences only — the profile library comes from the server in load().
    try {
      const raw = localStorage.getItem(LS_KEY);
      if (raw) {
        const p = JSON.parse(raw) as {
          enabled?: boolean;
          activeId?: string | null;
          levelingMode?: unknown;
          leveling?: boolean; // legacy boolean toggle (pre-album mode)
        };
        this.enabled = !!p.enabled;
        this.activeId = p.activeId ?? null;
        if (this.activeId) this.legacySelection = {profile_id:this.activeId, enabled:this.enabled};
        // Prefer the new mode; fall back to the legacy boolean (on → album, the Auto behavior).
        this.levelingMode = isLevelingMode(p.levelingMode)
          ? p.levelingMode
          : p.leveling
            ? "album"
            : "off";
      }
    } catch {
      // private mode / corrupt — start empty
    }
  }

  get leveling(): boolean {
    return this.levelingMode !== "off";
  }

  /** Fetch the server profile library; one-time migrate any legacy localStorage presets. */
  async load(): Promise<void> {
    try {
      let server = await api.dspProfiles();
      const legacy = this.legacyCustom();
      if (server.length === 0 && legacy.length > 0) {
        for (const p of legacy) await api.saveDspProfile(p);
        server = legacy;
      }
      this.custom = server;
      // Only now is it safe to rewrite localStorage without the legacy `custom` key — the
      // migration fully succeeded (or there was nothing to migrate / the server already had
      // profiles). On a failure above we fall to `catch` and leave localStorage intact so the
      // un-uploaded profiles survive for the next load's retry.
      this.saveLocal();
    } catch {
      this.custom = [];
    }
    this.loaded = true;
    if (outputAudio.state?.measurement_point === "browser_output" && !outputAudio.state.configured) void this.migrateLegacy();
  }

  get profiles(): EqProfile[] {
    return [...BUILT_IN_PROFILES.filter(p => !this.isCustom(p.id)), ...this.custom];
  }
  get active(): EqProfile | null {
    return this.profiles.find((p) => p.id === this.activeId) ?? null;
  }
  isCustom(id: string | null): boolean {
    return id != null && this.custom.some((p) => p.id === id);
  }

  setEnabled(v: boolean): void {
    if (v && !this.activeId) this.activeId = this.profiles[0]?.id ?? null;
    this.enabled = v && this.activeId !== null;
    this.write();
  }
  setLevelingMode(mode: LevelingMode): void {
    this.levelingMode = mode;
    this.saveLocal();
  }
  setActive(id: string | null): void {
    this.activeId = id;
    this.enabled = id !== null;
    this.write();
  }

  /** Persist a profile to the server library, select it, and enable. */
  async saveProfile(prof: EqProfile): Promise<void> {
    const output = this.outputId;
    await api.saveDspProfile(prof);
    this.custom = [...this.custom.filter((p) => p.id !== prof.id), prof];
    if (output === this.outputId) { this.activeId = prof.id; this.enabled = true; }
    this.write(output, {profile_id: prof.id, enabled: true});
  }

  /** Parse + save a ParametricEQ.txt preset. Returns null if nothing parsed. */
  async importText(text: string, name: string): Promise<EqProfile | null> {
    const prof = parseParametricEq(text, name);
    if (prof.bands.length === 0) return null;
    await this.saveProfile(prof);
    return prof;
  }

  async remove(id: string): Promise<void> {
    await api.deleteDspProfile(id);
    this.custom = this.custom.filter((p) => p.id !== id);
    if (this.activeId === id) this.activeId = null;
    this.saveLocal();
  }

  private saveLocal(): void {
    try {
      localStorage.setItem(
        LS_KEY,
        JSON.stringify({
          enabled: this.legacySelection?.enabled ?? false,
          activeId: this.legacySelection?.profile_id ?? null,
          levelingMode: this.levelingMode,
        }),
      );
    } catch {
      // private mode — fine
    }
  }

  private legacyCustom(): EqProfile[] {
    try {
      const p = JSON.parse(localStorage.getItem(LS_KEY) || "{}") as { custom?: EqProfile[] };
      return Array.isArray(p.custom) ? p.custom : [];
    } catch {
      return [];
    }
  }
}

export const dsp = new Dsp();

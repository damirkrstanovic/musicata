// SPDX-License-Identifier: AGPL-3.0-or-later
// EQ state (Svelte 5 runes). The **profile library** (`custom`) is now stored **server-side**
// (so presets follow the user across devices — see crate::dsp), loaded via `load()`. The
// Selection and bypass belong to each server output. Volume leveling and physical browser
// sink bindings remain local. The owning renderer applies the audio-channel configuration.
import { parseParametricEq, newProfileId, BUILT_IN_PROFILES, type EqProfile } from "./dsp";
import { api } from "./api";
import { session } from "./session.svelte";
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
  correctionEnabled = $state(true);
  listeningId = $state<string | null>(null);
  listeningEnabled = $state(false);
  effective = $state<EqProfile | null>(null);
  /** The server-stored profile library (built-ins live in `dsp.ts` and are merged in). */
  custom = $state<EqProfile[]>([]);
  panelOpen = $state(false);
  /** Volume leveling (EBU R128 normalization across tracks / albums). */
  levelingMode = $state<LevelingMode>("off");
  loaded = $state(false);
  outputId = $state<string | null>(null);
  error = $state<string | null>(null);
  private writes = Promise.resolve();
  private pendingLegacy: EqProfile[] = [];
  private legacySelection: OutputDspSelection | null = null;
  private migrating = false;

  bindOutput(id: string, name: string): void {
    this.outputId = id;
    this.enabled = false;
    this.activeId = null;
    this.correctionEnabled = true;
    this.listeningId = null;
    this.listeningEnabled = false;
    this.effective = null;
    this.error = null;
    outputAudio.select(id, name, config => this.adopt(config));
  }

  private adopt(config: AudioConfig): void {
    if (config.state.output_id !== this.outputId) return;
    if (!config.state.configured && config.state.measurement_point === "browser_output") void this.migrateLegacy();
    this.enabled = config.state.selection.enabled;
    this.activeId = config.state.selection.profile_id;
    this.correctionEnabled = config.state.selection.correction_enabled ?? true;
    this.listeningId = config.state.selection.listening_profile_id ?? null;
    this.listeningEnabled = config.state.selection.listening_enabled ?? false;
    this.effective = config.profile;
    // The effective renderer profile contains both layers; never put it in the raw library.
    for (const profile of [config.correction_profile, config.listening_profile]) {
      if (profile) this.custom = [...this.custom.filter(p => p.id !== profile.id), profile];
    }
  }

  private async migrateLegacy(): Promise<void> {
    if (!this.loaded || this.migrating || !this.legacySelection || !this.outputId) return;
    const id = this.outputId;
    const selection = this.legacySelection;
    const profile = this.profiles.find(p => p.id === selection.profile_id);
    if (!profile) return;
    this.migrating = true;
    try {
      await api.saveOutputDsp(id, selection, true);
      this.legacySelection = null;
      this.saveLocal();
    } catch { /* Preserve legacy preference for the next load. */ }
    finally {this.migrating = false;}
  }

  private selection(): OutputDspSelection {
    return {profile_id: this.activeId, enabled: this.enabled, correction_enabled: this.correctionEnabled,
      listening_profile_id: this.listeningId, listening_enabled: this.listeningEnabled};
  }

  private write(id = this.outputId, selection: OutputDspSelection = this.selection()): void {
    if (!id) return;
    const profiles = this.profiles.filter(p => p.id === selection.profile_id || p.id === selection.listening_profile_id);
    this.error = null;
    this.writes = this.writes.then(async () => {
      for (const profile of profiles) {
        if (!this.isCustom(profile.id)) {
          const existing = (await api.dspProfiles()).find(p => p.id === profile.id);
          if (!existing) await api.saveDspProfile(profile);
          this.custom = [...this.custom.filter(p => p.id !== profile.id), existing ?? profile];
        }
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
          custom?: EqProfile[];
          activeId?: string | null;
          levelingMode?: unknown;
          leveling?: boolean; // legacy boolean toggle (pre-album mode)
        };
        this.pendingLegacy = Array.isArray(p.custom) ? p.custom : [];
        this.enabled = !!p.enabled;
        this.activeId = p.activeId ?? null;
        if (this.activeId) this.legacySelection = {profile_id:this.activeId, enabled:this.enabled, correction_enabled:true, listening_profile_id:null, listening_enabled:false};
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
      const missing = this.pendingLegacy.filter(profile => !server.some(stored => stored.id === profile.id));
      if (missing.length > 0 && !session.isAdmin) {
        this.custom = server;
        this.error = "Your saved local sound profiles are preserved. Ask an administrator to sign in on this browser to import them before applying them.";
      } else {
        for (const profile of missing) await api.saveDspProfile(profile);
        server = [...server, ...missing];
        this.custom = server;
        this.pendingLegacy = [];
      }
      // Pending imports survive preference changes and reloads until an administrator saves them.
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
  isBuiltIn(id: string | null): boolean {
    return BUILT_IN_PROFILES.some(profile => profile.id === id);
  }
  isCustom(id: string | null): boolean {
    return id != null && this.custom.some((p) => p.id === id);
  }

  setEnabled(v: boolean): void {
    if (v && !this.activeId && !this.listeningId) {
      this.activeId = this.profiles.find(profile => profile.kind !== "listening")?.id ?? null;
      this.correctionEnabled = true;
    }
    this.enabled = v && !!((this.correctionEnabled && this.activeId) || (this.listeningEnabled && this.listeningId));
    this.write();
  }
  setCorrectionEnabled(v: boolean): void {
    this.correctionEnabled = v && this.activeId !== null;
    this.enabled = !!((this.correctionEnabled && this.activeId) || (this.listeningEnabled && this.listeningId));
    this.write();
  }
  setListeningEnabled(v: boolean): void {
    this.listeningEnabled = v && this.listeningId !== null;
    this.enabled = !!((this.correctionEnabled && this.activeId) || (this.listeningEnabled && this.listeningId));
    this.write();
  }
  setListening(id: string | null): void {
    this.listeningId = id;
    this.listeningEnabled = id !== null;
    this.enabled = !!((this.correctionEnabled && this.activeId) || this.listeningEnabled);
    this.write();
  }
  setLevelingMode(mode: LevelingMode): void {
    this.levelingMode = mode;
    this.saveLocal();
  }
  setActive(id: string | null): void {
    this.activeId = id;
    this.correctionEnabled = id !== null;
    this.enabled = !!(id || (this.listeningEnabled && this.listeningId));
    this.write();
  }

  /** Save a profile; administration can preserve the current listening selection. */
  async saveProfile(prof: EqProfile, activate = true): Promise<void> {
    const output = this.outputId;
    const selection = {...this.selection(), profile_id: prof.id, correction_enabled: true, enabled: true};
    await api.saveDspProfile(prof);
    this.custom = [...this.custom.filter((p) => p.id !== prof.id), prof];
    if (!activate) return;
    if (output === this.outputId) { this.activeId = prof.id; this.correctionEnabled = true; this.enabled = true; }
    this.write(output, selection);
  }

  /** Parse + save a ParametricEQ.txt preset. Returns null if nothing parsed. */
  async importText(text: string, name: string, activate = true, kind?: EqProfile["kind"]): Promise<EqProfile | null> {
    const prof = parseParametricEq(text, name);
    if (prof.bands.length === 0) return null;
    if (!activate) prof.id = newProfileId();
    if (kind) prof.kind = kind;
    await this.saveProfile(prof, activate);
    return prof;
  }

  async remove(id: string): Promise<void> {
    await api.deleteDspProfile(id);
    this.custom = this.custom.filter((p) => p.id !== id);
    if (this.activeId === id) { this.activeId = null; this.correctionEnabled = false; }
    if (this.listeningId === id) { this.listeningId = null; this.listeningEnabled = false; }
    this.enabled = this.enabled && !!((this.correctionEnabled && this.activeId) || (this.listeningEnabled && this.listeningId));
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
          ...(this.pendingLegacy.length ? {custom: this.pendingLegacy} : {}),
        }),
      );
    } catch {
      // private mode — fine
    }
  }
}

export const dsp = new Dsp();

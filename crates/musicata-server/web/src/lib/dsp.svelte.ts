// EQ state (Svelte 5 runes). The **profile library** (`custom`) is now stored **server-side**
// (so presets follow the user across devices — see crate::dsp), loaded via `load()`. The
// per-browser bits — which profile is active, the enabled flag, and volume leveling — stay in
// localStorage (they're output/device preferences). The active profile is pushed into the Web
// Audio graph by an $effect in player/App.svelte.
import { parseParametricEq, BUILT_IN_PROFILES, type EqProfile } from "./dsp";
import { api } from "./api";

const LS_KEY = "musicata-dsp"; // client prefs (+ legacy `custom`, migrated to the server once)

class Dsp {
  enabled = $state(false);
  activeId = $state<string | null>(null);
  /** The server-stored profile library (built-ins live in `dsp.ts` and are merged in). */
  custom = $state<EqProfile[]>([]);
  panelOpen = $state(false);
  /** Volume leveling (EBU R128 normalization across tracks). */
  leveling = $state(false);
  loaded = $state(false);

  constructor() {
    // Client preferences only — the profile library comes from the server in load().
    try {
      const raw = localStorage.getItem(LS_KEY);
      if (raw) {
        const p = JSON.parse(raw) as { enabled?: boolean; activeId?: string | null; leveling?: boolean };
        this.enabled = !!p.enabled;
        this.activeId = p.activeId ?? null;
        this.leveling = !!p.leveling;
      }
    } catch {
      // private mode / corrupt — start empty
    }
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
  }

  get profiles(): EqProfile[] {
    return [...BUILT_IN_PROFILES, ...this.custom];
  }
  get active(): EqProfile | null {
    return this.profiles.find((p) => p.id === this.activeId) ?? null;
  }
  isCustom(id: string | null): boolean {
    return id != null && this.custom.some((p) => p.id === id);
  }

  setEnabled(v: boolean): void {
    this.enabled = v;
    this.saveLocal();
  }
  setLeveling(v: boolean): void {
    this.leveling = v;
    this.saveLocal();
  }
  setActive(id: string | null): void {
    this.activeId = id;
    if (id && !this.enabled) this.enabled = true;
    this.saveLocal();
  }

  /** Persist a profile to the server library, select it, and enable. */
  async saveProfile(prof: EqProfile): Promise<void> {
    await api.saveDspProfile(prof);
    this.custom = [...this.custom.filter((p) => p.id !== prof.id), prof];
    this.activeId = prof.id;
    this.enabled = true;
    this.saveLocal();
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
        JSON.stringify({ enabled: this.enabled, activeId: this.activeId, leveling: this.leveling }),
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

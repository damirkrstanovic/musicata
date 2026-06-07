// Client-side EQ state (Svelte 5 runes). Per-browser: the active profile + enabled flag +
// any imported presets persist to localStorage (correction profiles are device/output-bound,
// not library data). The active profile is pushed into the Web Audio graph by an $effect in
// player/App.svelte. Server-side profile sync (so presets follow the user across devices) is
// a planned follow-up — see docs/dsp.md.
import { parseParametricEq, BUILT_IN_PROFILES, type EqProfile } from "./dsp";

const LS_KEY = "musicata-dsp";

interface Persisted {
  enabled: boolean;
  activeId: string | null;
  custom: EqProfile[];
}

class Dsp {
  enabled = $state(false);
  activeId = $state<string | null>(null);
  /** User-imported presets (built-ins live in `dsp.ts`). */
  custom = $state<EqProfile[]>([]);
  panelOpen = $state(false);

  constructor() {
    try {
      const raw = localStorage.getItem(LS_KEY);
      if (raw) {
        const p = JSON.parse(raw) as Persisted;
        this.enabled = !!p.enabled;
        this.activeId = p.activeId ?? null;
        this.custom = Array.isArray(p.custom) ? p.custom : [];
      }
    } catch {
      // private mode / corrupt — start empty
    }
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
    this.save();
  }
  setActive(id: string | null): void {
    this.activeId = id;
    if (id && !this.enabled) this.enabled = true;
    this.save();
  }
  /** Parse + add a ParametricEQ.txt preset, select it, and enable. Returns null if nothing parsed. */
  importText(text: string, name: string): EqProfile | null {
    const prof = parseParametricEq(text, name);
    if (prof.bands.length === 0) return null;
    this.custom = [...this.custom.filter((p) => p.id !== prof.id), prof];
    this.activeId = prof.id;
    this.enabled = true;
    this.save();
    return prof;
  }
  remove(id: string): void {
    this.custom = this.custom.filter((p) => p.id !== id);
    if (this.activeId === id) this.activeId = null;
    this.save();
  }

  private save(): void {
    try {
      const data: Persisted = { enabled: this.enabled, activeId: this.activeId, custom: this.custom };
      localStorage.setItem(LS_KEY, JSON.stringify(data));
    } catch {
      // private mode — fine
    }
  }
}

export const dsp = new Dsp();

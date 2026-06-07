// Tiny UI state for the VU meter overlay (open/closed), persisted so it stays where the user
// left it. Levels themselves are read live from the audio graph (see lib/audio.ts `levels()`).
const LS_KEY = "musicata-vu-open";

class Meter {
  open = $state(false);

  constructor() {
    try {
      this.open = localStorage.getItem(LS_KEY) === "1";
    } catch {
      // private mode — fine
    }
  }

  toggle(): void {
    this.open = !this.open;
    try {
      localStorage.setItem(LS_KEY, this.open ? "1" : "0");
    } catch {
      // private mode — fine
    }
  }
}

export const meter = new Meter();

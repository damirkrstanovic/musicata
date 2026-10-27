// SPDX-License-Identifier: AGPL-3.0-or-later
// Continuous-play ("keep the music going") toggle. Server-side state (the autoplay loop reads
// it), surfaced here for the queue-drawer toggle. Optimistic with rollback on failure.
import { api } from "./api";

class Autoplay {
  enabled = $state<boolean | null>(null);
  busy = $state(false);
  error = $state<string | null>(null);

  async load(): Promise<void> {
    this.error = null;
    try {
      this.enabled = (await api.autoplay()).enabled;
    } catch {
      this.error = "Could not load continuous play. Try again.";
    }
  }

  async toggle(): Promise<void> {
    if (this.enabled === null || this.busy) return;
    const next = !this.enabled;
    this.enabled = next;
    this.busy = true;
    this.error = null;
    try {
      await api.setAutoplay(next);
    } catch {
      this.enabled = !next; // rollback
      this.error = "Could not update continuous play. Try again.";
    } finally {
      this.busy = false;
    }
  }
}

export const autoplay = new Autoplay();

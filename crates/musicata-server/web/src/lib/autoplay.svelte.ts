// SPDX-License-Identifier: AGPL-3.0-or-later
// Continuous-play ("keep the music going") toggle. Server-side state (the autoplay loop reads
// it), surfaced here for the queue-drawer toggle. Optimistic with rollback on failure.
import { api } from "./api";

class Autoplay {
  enabled = $state(false);

  async load(): Promise<void> {
    try {
      this.enabled = (await api.autoplay()).enabled;
    } catch {
      // keep default
    }
  }

  async toggle(): Promise<void> {
    const next = !this.enabled;
    this.enabled = next;
    try {
      await api.setAutoplay(next);
    } catch {
      this.enabled = !next; // rollback
    }
  }
}

export const autoplay = new Autoplay();

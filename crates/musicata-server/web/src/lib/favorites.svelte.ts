// SPDX-License-Identifier: AGPL-3.0-or-later
// Tracks the set of favorited track ids so any track row can show its heart state, with an
// optimistic toggle. Loaded once on startup.
import { api } from "./api";

class Favorites {
  trackIds = $state<Set<string>>(new Set());

  async load(): Promise<void> {
    try {
      const favs = await api.favorites();
      this.trackIds = new Set(favs.tracks.map((t) => t.id));
    } catch {
      // leave empty
    }
  }

  has(id: string): boolean {
    return this.trackIds.has(id);
  }

  toggleTrack(id: string): void {
    const wasFavorite = this.trackIds.has(id);
    const next = new Set(this.trackIds);
    if (wasFavorite) next.delete(id);
    else next.add(id);
    this.trackIds = next;

    // Optimistic, but reconcile on failure so the heart doesn't lie about saved state.
    const request = wasFavorite ? api.unstar("track", id) : api.star("track", id);
    request.catch(() => {
      const reverted = new Set(this.trackIds);
      if (wasFavorite) reverted.add(id);
      else reverted.delete(id);
      this.trackIds = reverted;
    });
  }
}

export const favorites = new Favorites();

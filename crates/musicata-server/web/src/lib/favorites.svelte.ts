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
    const next = new Set(this.trackIds);
    if (next.has(id)) {
      next.delete(id);
      api.unstar("track", id).catch(() => {});
    } else {
      next.add(id);
      api.star("track", id).catch(() => {});
    }
    this.trackIds = next;
  }
}

export const favorites = new Favorites();

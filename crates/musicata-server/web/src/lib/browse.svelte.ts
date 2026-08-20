// SPDX-License-Identifier: AGPL-3.0-or-later
// The active library filter (browse facets) + the facet index. LibraryGrid reads the filter
// and refetches when it changes.
import { api, type BrowseIndex } from "./api";

class Browse {
  index = $state<BrowseIndex | null>(null);
  genre = $state("");
  year = $state<number | null>(null);
  composer = $state("");
  tag = $state("");

  async load(): Promise<void> {
    try {
      this.index = await api.browse();
    } catch {
      // facets are optional
    }
  }

  get active(): boolean {
    return !!this.genre || this.year !== null || !!this.composer || !!this.tag;
  }

  reset(): void {
    this.genre = "";
    this.year = null;
    this.composer = "";
    this.tag = "";
  }
}

export const browse = new Browse();

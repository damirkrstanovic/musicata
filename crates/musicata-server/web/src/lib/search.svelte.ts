// SPDX-License-Identifier: AGPL-3.0-or-later
import { nav } from "./nav.svelte";

class Search {
  query = $state("");
  draft = $state("");
  private timer: ReturnType<typeof setTimeout> | undefined;

  cancelPending(): void { clearTimeout(this.timer); }
  schedule(value: string): void {
    this.draft = value;
    this.cancelPending();
    const route = JSON.stringify(nav.current);
    this.timer = setTimeout(() => {
      if (JSON.stringify(nav.current) === route) this.update(value);
      else this.draft = this.query;
    }, 220);
  }
  private origin: ReturnType<typeof nav.captureBrowse> | null = null;

  update(value: string): void {
    this.cancelPending();
    if (!value.trim()) { this.clear(); return; }
    if (!this.query.trim()) this.origin = nav.captureBrowse();
    this.query = value;
    this.draft = value;
    if (!["library", "tracks", "artists"].includes(nav.current.name)) nav.root({name: nav.browseView});
  }

  clear(): void {
    this.cancelPending();
    this.query = "";
    this.draft = "";
    const origin = this.origin;
    this.origin = null;
    if (origin) nav.returnToBrowse(origin);
    else nav.root({name: nav.browseView});
  }
}
export const search = new Search();

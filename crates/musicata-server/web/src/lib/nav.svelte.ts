// SPDX-License-Identifier: AGPL-3.0-or-later
// Browser history is the source of truth for routes and temporary playback/drawer views.
import type { SettingsCategory } from "./settings-nav";
export type Route =
  | { name: "tracks" }
  | { name: "library" }
  | { name: "artists" }
  | { name: "favorites" }
  | { name: "playlists" }
  | { name: "radio" }
  | { name: "mix" }
  | { name: "queue" }
  | { name: "settings"; category: SettingsCategory }
  | { name: "album"; id: string; title: string }
  | { name: "artist"; id: string; label: string }
  | { name: "playlist"; id: string; label: string }
  | { name: "smart"; id: string; label: string };

type Overlay = "navigation" | "nowPlaying" | "queue";
type Scroll = { page: number; content: number };
type Entry = {
  musicata: 1;
  index: number;
  route: Route;
  overlays: Overlay[];
  scroll: Scroll;
};

class Nav {
  current = $state<Route>({ name: "library" });
  overlays = $state<Overlay[]>([]);
  private index = $state(0);
  private pendingScroll: Scroll | null = null;
  private observer: MutationObserver | null = null;
  private frame = 0;
  private positions = new Map<number, Scroll>();
  private rememberScroll = () => {
    if (!this.pendingScroll) this.positions.set(this.index, this.snapshot().scroll);
  };
  private cancelScrollRestore = () => { this.pendingScroll = null; };
  private initial: Route = { name: "library" };
  browseView = $state<"library" | "tracks" | "artists">("library");

  get canGoBack(): boolean { return this.index > 0; }

  start(): void {
    try {
      const saved = localStorage.getItem("musicata.browse-view");
      if (saved === "library" || saved === "tracks" || saved === "artists") this.browseView = saved;
    } catch { /* Browsing still works when storage is unavailable. */ }
    this.initial = location.pathname === "/admin" ? {name: "settings", category: "sources"} : { name: this.browseView };
    history.scrollRestoration = "manual";
    window.addEventListener("scroll", this.rememberScroll, true);
    window.addEventListener("wheel", this.cancelScrollRestore, {passive: true});
    window.addEventListener("touchstart", this.cancelScrollRestore, {passive: true});
    this.restore(history.state);
    this.observer = new MutationObserver(() => this.scheduleScroll());
    const content = document.querySelector('.content');
    if (content) this.observer.observe(content, {childList: true, subtree: true});
  }

  stop(): void {
    this.observer?.disconnect();
    window.removeEventListener("scroll", this.rememberScroll, true);
    window.removeEventListener("wheel", this.cancelScrollRestore);
    window.removeEventListener("touchstart", this.cancelScrollRestore);
    cancelAnimationFrame(this.frame);
    history.scrollRestoration = "auto";
  }

  private snapshot(): Entry {
    return {musicata: 1, index: this.index, route: {...this.current},
      overlays: [...this.overlays], scroll: {
        page: window.scrollY,
        content: document.querySelector('.content')?.scrollTop ?? 0,
      }};
  }

  private save(): void {
    const entry = this.snapshot();
    this.positions.set(this.index, entry.scroll);
    history.replaceState(entry, "");
  }

  private apply(entry: Entry): void {
    this.current = entry.route;
    const name = entry.route.name;
    if (name === "library" || name === "tracks" || name === "artists") {
      this.browseView = name;
      try { localStorage.setItem("musicata.browse-view", name); } catch { /* Optional preference. */ }
    }
    this.overlays = entry.overlays;
    this.index = entry.index;
    this.pendingScroll = entry.scroll;
    this.scheduleScroll();
  }

  restore(state: Entry | null): void {
    const entry: Entry = state?.musicata === 1 ? state : {
      musicata: 1, index: 0, route: this.initial, overlays: [], scroll: {page: 0, content: 0},
    };
    entry.scroll = this.positions.get(entry.index) ?? entry.scroll;
    this.apply(entry);
    history.replaceState(entry, "");
  }

  private scheduleScroll(): void {
    cancelAnimationFrame(this.frame);
    this.frame = requestAnimationFrame(() => this.restoreScroll());
  }

  // Called again after async content arrives. Paged views load further pages
  // while the saved position is beyond their currently rendered content.
  restoreScroll(): boolean {
    const goal = this.pendingScroll;
    if (!goal) return true;
    window.scrollTo({top: goal.page, behavior: "instant"});
    const content = document.querySelector('.content');
    content?.scrollTo({top: goal.content, behavior: "instant"});
    const restored = Math.abs(window.scrollY - goal.page) < 2
      && Math.abs((content?.scrollTop ?? 0) - goal.content) < 2;
    if (restored) this.pendingScroll = null;
    return restored;
  }

  /** Both root choices and detail views participate in browser Back/Forward. */
  root(route: Route): void { this.push(route); }

  push(route: Route): void {
    if (JSON.stringify(route) === JSON.stringify(this.current)) {
      if (this.overlays.length) this.pop();
      return;
    }
    this.save();
    const replace = this.overlays.length > 0;
    const entry: Entry = {musicata: 1, index: this.index + (replace ? 0 : 1),
      route, overlays: [], scroll: {page: 0, content: 0}};
    // Selecting a destination from a drawer replaces that temporary drawer entry.
    if (replace) history.replaceState(entry, "");
    else history.pushState(entry, "");
    this.positions.set(entry.index, entry.scroll);
    this.apply(entry);
  }

  openOverlay(overlay: Overlay): void {
    if (this.overlays.includes(overlay)) return;
    this.save();
    const entry = this.snapshot();
    entry.index++;
    entry.overlays.push(overlay);
    history.pushState(entry, "");
    this.positions.set(entry.index, entry.scroll);
    this.apply(entry);
  }

  closeOverlay(): void { if (this.overlays.length) this.pop(); }

  pop(): void { if (this.canGoBack) history.back(); }
}

export const nav = new Nav();

export function activityFor(route: Route): "browse" | "listen" | "settings" {
  if (route.name === "settings") return "settings";
  if (["queue", "playlists", "playlist", "smart", "radio", "mix"].includes(route.name)) return "listen";
  return "browse";
}

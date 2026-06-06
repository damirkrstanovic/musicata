// In-page navigation: a stack of routes synced to the History API for the Back button,
// mirroring the old player's nav-stack (the player is one page, not a client router).
export type Route =
  | { name: "tracks" }
  | { name: "library" }
  | { name: "artists" }
  | { name: "favorites" }
  | { name: "playlists" }
  | { name: "radio" }
  | { name: "search" }
  | { name: "album"; id: string; title: string }
  | { name: "artist"; id: string; label: string }
  | { name: "playlist"; id: string; label: string }
  | { name: "smart"; id: string; label: string };

class Nav {
  stack = $state<Route[]>([{ name: "tracks" }]);

  get current(): Route {
    return this.stack[this.stack.length - 1];
  }
  get canGoBack(): boolean {
    return this.stack.length > 1;
  }

  /** Switch the root view (Library/Artists tab) — resets the stack. */
  root(route: Route): void {
    this.stack = [route];
  }
  /** Drill into a detail view. */
  push(route: Route): void {
    this.stack = [...this.stack, route];
    history.pushState({ depth: this.stack.length }, "");
  }
  pop(): void {
    if (this.stack.length > 1) this.stack = this.stack.slice(0, -1);
  }
}

export const nav = new Nav();

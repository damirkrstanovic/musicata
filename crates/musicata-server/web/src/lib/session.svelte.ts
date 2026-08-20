// SPDX-License-Identifier: AGPL-3.0-or-later
// The signed-in user, shared across the app. Drives the auth gate: setup (no accounts yet) →
// login → app. A 401 from any request (dispatched by api.ts) drops us back to the login screen.
import { api, type SessionUser } from "./api";

class Session {
  user = $state<SessionUser | null>(null);
  /** No accounts exist yet — show the create-admin screen instead of login. */
  setupRequired = $state(false);
  /** The initial status/me probe has completed (so the gate can stop showing a blank). */
  ready = $state(false);

  constructor() {
    // Registered once (this is a module singleton), so a 401 from any request — including
    // during init — drops us to login, and the listener never stacks across init() calls.
    window.addEventListener("musicata:unauthorized", () => {
      this.user = null;
    });
  }

  get isAdmin(): boolean {
    return this.user?.role === "admin";
  }

  async init(): Promise<void> {
    try {
      const status = await api.authStatus();
      this.setupRequired = status.setup_required;
      if (!status.setup_required) {
        try {
          this.user = await api.me();
        } catch {
          this.user = null; // not signed in
        }
      }
    } catch {
      // Network/feature error — leave defaults; the gate will show login.
    }
    this.ready = true;
  }

  async login(username: string, password: string): Promise<void> {
    const result = await api.login(username, password);
    this.user = result?.user ?? null;
    this.setupRequired = false;
  }

  async setup(username: string, password: string): Promise<void> {
    const result = await api.setup(username, password);
    this.user = result?.user ?? null;
    this.setupRequired = false;
  }

  async logout(): Promise<void> {
    try {
      await api.logout();
    } finally {
      this.user = null;
    }
  }
}

export const session = new Session();

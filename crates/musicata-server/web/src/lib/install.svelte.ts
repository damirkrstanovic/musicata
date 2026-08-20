// SPDX-License-Identifier: AGPL-3.0-or-later
// PWA install affordance. Two paths: capture the deferred `beforeinstallprompt` (Chrome/Android)
// so we can offer an in-app "Install" button, and detect iOS Safari — which never fires that
// event — to show a one-time "Add to Home Screen" hint. In-product UI only, no native popups.

interface BeforeInstallPromptEvent extends Event {
  prompt: () => Promise<void>;
  userChoice: Promise<{ outcome: "accepted" | "dismissed" }>;
}

const IOS_HINT_KEY = "musicata.iosInstallHintDismissed";

function isStandalone(): boolean {
  return (
    window.matchMedia?.("(display-mode: standalone)").matches ||
    // iOS Safari's non-standard flag
    (navigator as unknown as { standalone?: boolean }).standalone === true
  );
}

function isIos(): boolean {
  const ua = navigator.userAgent;
  const iPhone = /iPad|iPhone|iPod/.test(ua);
  // iPadOS 13+ reports as a Mac; a touch-capable "Mac" is really an iPad.
  const iPadOS = navigator.platform === "MacIntel" && navigator.maxTouchPoints > 1;
  return iPhone || iPadOS;
}

class Install {
  private deferred: BeforeInstallPromptEvent | null = $state(null);
  installed = $state(false);
  /** Show the iOS "Add to Home Screen" hint (iOS Safari, not installed, not dismissed). */
  iosHint = $state(false);

  /** True when the browser offered an installable prompt we can fire from a button. */
  get canPrompt(): boolean {
    return this.deferred !== null && !this.installed;
  }

  init(): void {
    if (isStandalone()) {
      this.installed = true;
      return;
    }
    window.addEventListener("beforeinstallprompt", (event) => {
      event.preventDefault(); // stash it for our own button instead of the mini-infobar
      this.deferred = event as BeforeInstallPromptEvent;
    });
    window.addEventListener("appinstalled", () => {
      this.installed = true;
      this.deferred = null;
      this.iosHint = false;
    });
    if (isIos() && localStorage.getItem(IOS_HINT_KEY) !== "1") {
      this.iosHint = true;
    }
  }

  async prompt(): Promise<void> {
    const event = this.deferred;
    if (!event) return;
    this.deferred = null; // a prompt can only be used once
    await event.prompt();
    await event.userChoice.catch(() => undefined);
  }

  dismissIosHint(): void {
    this.iosHint = false;
    try {
      localStorage.setItem(IOS_HINT_KEY, "1");
    } catch {
      // ignore storage failures (e.g. iOS private mode)
    }
  }
}

export const install = new Install();

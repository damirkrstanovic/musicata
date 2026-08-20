<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  // A slim, dismissable top banner offering PWA install: a one-tap "Install" on Chrome/Android,
  // or "Add to Home Screen" instructions on iOS Safari (which has no install API).
  import { install } from "../lib/install.svelte";
</script>

{#if install.iosHint}
  <div class="install-banner" role="dialog" aria-label="Install Musicata">
    <div class="install-text">
      <strong>Install Musicata</strong>
      <span>Tap <span aria-hidden="true">⎙</span> Share, then “Add to Home Screen”.</span>
    </div>
    <button type="button" class="install-x" onclick={() => install.dismissIosHint()} aria-label="Dismiss">×</button>
  </div>
{:else if install.canPrompt}
  <div class="install-banner">
    <div class="install-text">
      <strong>Install Musicata</strong>
      <span>Add it to your device for a full-screen app.</span>
    </div>
    <button type="button" class="install-go" onclick={() => install.prompt()}>Install</button>
  </div>
{/if}

<style>
  .install-banner {
    position: fixed;
    top: max(0.5rem, env(safe-area-inset-top));
    left: 50%;
    transform: translateX(-50%);
    z-index: 60;
    display: flex;
    align-items: center;
    gap: 0.75rem;
    max-width: min(28rem, calc(100vw - 1rem));
    padding: 0.6rem 0.75rem;
    border-radius: 0.75rem;
    background: #20242c;
    border: 1px solid rgba(255, 255, 255, 0.08);
    box-shadow: 0 8px 24px rgba(0, 0, 0, 0.4);
    color: #e9ecf1;
  }
  .install-text {
    display: flex;
    flex-direction: column;
    gap: 0.1rem;
    font-size: 0.85rem;
    line-height: 1.2;
  }
  .install-text span {
    color: #aab1bd;
  }
  .install-go {
    margin-left: auto;
    padding: 0.4rem 0.8rem;
    border-radius: 0.5rem;
    border: none;
    background: #d4af37;
    color: #16181d;
    font-weight: 600;
    cursor: pointer;
  }
  .install-x {
    margin-left: auto;
    background: transparent;
    border: none;
    color: #aab1bd;
    font-size: 1.3rem;
    line-height: 1;
    cursor: pointer;
    padding: 0 0.25rem;
  }
  @media (prefers-reduced-motion: no-preference) {
    .install-banner {
      animation: install-in 0.2s ease-out;
    }
  }
  @keyframes install-in {
    from {
      opacity: 0;
      transform: translate(-50%, -0.5rem);
    }
    to {
      opacity: 1;
      transform: translate(-50%, 0);
    }
  }
</style>

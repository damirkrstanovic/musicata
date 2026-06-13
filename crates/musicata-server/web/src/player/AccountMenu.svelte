<script lang="ts">
  // Compact account control for the player: who's signed in, plus self-service password change,
  // Subsonic-token reveal, and sign out — reusing the shared modal primitives (no new overlay).
  import { api, ApiError } from "../lib/api";
  import { session } from "../lib/session.svelte";
  import { openModal } from "../lib/modal";

  let open = $state(false);

  async function changePassword() {
    open = false;
    const result = await openModal({
      title: "Change password",
      confirmLabel: "Change",
      fields: [
        { key: "current", label: "Current password", type: "password" },
        { key: "next", label: "New password", type: "password" },
      ],
    });
    if (!result) return;
    try {
      await api.changePassword(result.current, result.next);
      await openModal({ title: "Password changed", message: "Done." });
    } catch (e) {
      await openModal({ title: "Couldn't change password", message: e instanceof ApiError ? e.message : String(e) });
    }
  }

  async function showToken() {
    open = false;
    try {
      const { api_token } = await api.apiToken();
      await openModal({
        title: "Subsonic / API token",
        message: "Use your username and this token to connect a Subsonic app.",
        fields: [{ key: "token", label: "Token", value: api_token }],
        confirmLabel: "Done",
      });
    } catch (e) {
      await openModal({ title: "Couldn't load token", message: e instanceof ApiError ? e.message : String(e) });
    }
  }
</script>

<div class="account">
  <button type="button" class="account-btn" onclick={() => (open = !open)} aria-expanded={open}>
    {session.user?.username ?? "Account"}
  </button>
  {#if open}
    <div class="account-menu">
      <button type="button" onclick={changePassword}>Change password</button>
      <button type="button" onclick={showToken}>Subsonic token</button>
      <button type="button" class="danger" onclick={() => session.logout()}>Sign out</button>
    </div>
  {/if}
</div>

<style>
  .account {
    position: relative;
  }
  .account-btn {
    background: transparent;
    border: 1px solid rgba(255, 255, 255, 0.14);
    border-radius: 0.5rem;
    color: inherit;
    padding: 0.3rem 0.6rem;
    font-size: 0.8rem;
    cursor: pointer;
    max-width: 9rem;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .account-menu {
    position: absolute;
    right: 0;
    top: calc(100% + 0.3rem);
    z-index: 40;
    display: flex;
    flex-direction: column;
    min-width: 11rem;
    background: #20242c;
    border: 1px solid rgba(255, 255, 255, 0.1);
    border-radius: 0.5rem;
    box-shadow: 0 8px 24px rgba(0, 0, 0, 0.4);
    overflow: hidden;
  }
  .account-menu button {
    background: transparent;
    border: none;
    color: inherit;
    text-align: left;
    padding: 0.55rem 0.7rem;
    font-size: 0.82rem;
    cursor: pointer;
  }
  .account-menu button:hover {
    background: rgba(255, 255, 255, 0.06);
  }
  .account-menu button.danger {
    color: #e06b6b;
  }
</style>

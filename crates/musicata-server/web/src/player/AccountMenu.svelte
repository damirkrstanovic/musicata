<script lang="ts">
  // Bottom-left profile control for the player: who's signed in, plus a link to the admin page
  // (admins only), self-service password change, Subsonic-token reveal, and sign out — reusing
  // the shared modal primitives (no new overlay). The menu opens upward since it sits at the
  // bottom of the sidebar.
  import { api, ApiError } from "../lib/api";
  import { session } from "../lib/session.svelte";
  import { openModal } from "../lib/modal";

  // AGPL section 13: anyone interacting with Musicata over a network must be offered its
  // source. If you run a MODIFIED Musicata, repoint this at your own fork.
  const SOURCE_URL = "https://github.com/damirkrstanovic/musicata";

  let open = $state(false);

  const username = $derived(session.user?.username ?? "Account");
  const initial = $derived((session.user?.username?.[0] ?? "?").toUpperCase());

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
  {#if open}
    <div class="account-menu" role="menu">
      {#if session.isAdmin}
        <a class="item" href="/admin">Admin</a>
      {/if}
      <button type="button" class="item" onclick={changePassword}>Change password</button>
      <button type="button" class="item" onclick={showToken}>Subsonic token</button>
      <a class="item" href={SOURCE_URL} target="_blank" rel="noopener noreferrer">Source code</a>
      <button type="button" class="item danger" onclick={() => session.logout()}>Sign out</button>
    </div>
  {/if}
  <button type="button" class="profile" onclick={() => (open = !open)} aria-expanded={open} aria-haspopup="menu">
    <span class="avatar" aria-hidden="true">{initial}</span>
    <span class="who">{username}</span>
    <span class="chev" aria-hidden="true">{open ? "⌄" : "⌃"}</span>
  </button>
</div>

<style>
  .account {
    position: relative;
    margin-top: auto; /* push the profile to the bottom of the sidebar column */
  }
  .profile {
    display: flex;
    align-items: center;
    gap: 0.6rem;
    width: 100%;
    background: transparent;
    border: 1px solid var(--line, rgba(255, 255, 255, 0.12));
    border-radius: 0.6rem;
    color: inherit;
    padding: 0.45rem 0.55rem;
    cursor: pointer;
    text-align: left;
  }
  .profile:hover {
    background: rgba(255, 255, 255, 0.05);
  }
  .avatar {
    flex: none;
    display: grid;
    place-items: center;
    width: 1.9rem;
    height: 1.9rem;
    border-radius: 50%;
    background: var(--accent, #4f8cff);
    color: #fff;
    font-size: 0.85rem;
    font-weight: 600;
  }
  .who {
    flex: 1 1 auto;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-size: 0.85rem;
  }
  .chev {
    flex: none;
    opacity: 0.6;
    font-size: 0.85rem;
  }
  .account-menu {
    position: absolute;
    left: 0;
    right: 0;
    bottom: calc(100% + 0.3rem); /* open upward */
    z-index: 40;
    display: flex;
    flex-direction: column;
    background: #20242c;
    border: 1px solid rgba(255, 255, 255, 0.1);
    border-radius: 0.5rem;
    box-shadow: 0 8px 24px rgba(0, 0, 0, 0.4);
    overflow: hidden;
  }
  .account-menu .item {
    background: transparent;
    border: none;
    color: inherit;
    text-decoration: none;
    text-align: left;
    padding: 0.55rem 0.7rem;
    font-size: 0.82rem;
    cursor: pointer;
  }
  .account-menu .item:hover {
    background: rgba(255, 255, 255, 0.06);
  }
  .account-menu .item.danger {
    color: #e06b6b;
  }
</style>

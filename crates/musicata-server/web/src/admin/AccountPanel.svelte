<script lang="ts">
  import { api, ApiError } from "../lib/api";
  import { session } from "../lib/session.svelte";

  let current = $state("");
  let next = $state("");
  let pwMessage = $state("");
  let pwError = $state(false);
  let busy = $state(false);

  let token = $state("");
  let tokenMessage = $state("");

  async function changePassword(event: SubmitEvent) {
    event.preventDefault();
    busy = true;
    pwMessage = "";
    pwError = false;
    try {
      await api.changePassword(current, next);
      current = "";
      next = "";
      pwMessage = "Password changed.";
    } catch (e) {
      pwMessage = e instanceof ApiError ? e.message : String(e);
      pwError = true;
    } finally {
      busy = false;
    }
  }

  async function revealToken() {
    try {
      token = (await api.apiToken()).api_token;
      tokenMessage = "";
    } catch (e) {
      tokenMessage = e instanceof ApiError ? e.message : String(e);
    }
  }

  async function rotateToken() {
    try {
      token = (await api.rotateApiToken())?.api_token ?? "";
      tokenMessage = "Rotated — update your Subsonic apps.";
    } catch (e) {
      tokenMessage = e instanceof ApiError ? e.message : String(e);
    }
  }
</script>

<section class="admin-panel">
  <div class="admin-panel-head"><h2>Your account</h2></div>
  <p class="admin-hint">
    Signed in as <strong>{session.user?.username}</strong>
    <span class="role">{session.user?.role}</span>
  </p>

  <div class="admin-subhead">Change password</div>
  <form class="field-form" onsubmit={changePassword}>
    <label class="field"><span>Current password</span><input type="password" bind:value={current} required /></label>
    <label class="field"><span>New password</span><input type="password" bind:value={next} required /></label>
    <div class="field-actions">
      <button type="submit" class="primary-button" disabled={busy}>Change password</button>
      <span class="form-status" class:error={pwError}>{pwMessage}</span>
    </div>
  </form>

  <div class="admin-subhead">Subsonic / API token</div>
  <p class="admin-hint">
    Use your username and this token (as the password) to connect a Subsonic app, or append
    <code>?token=…</code> to API requests.
  </p>
  <div class="token-row">
    <button type="button" class="ghost-button" onclick={revealToken}>Reveal token</button>
    <button type="button" class="ghost-button" onclick={rotateToken}>Rotate</button>
  </div>
  {#if token}
    <input class="token" readonly value={token} onclick={(e) => (e.currentTarget as HTMLInputElement).select()} />
  {/if}
  {#if tokenMessage}<p class="admin-hint">{tokenMessage}</p>{/if}

  <div class="admin-subhead">Session</div>
  <button type="button" class="ghost-button danger" onclick={() => session.logout()}>Sign out</button>
</section>

<style>
  .admin-subhead {
    margin: 1rem 0 0.4rem;
    font-size: 0.78rem;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    opacity: 0.6;
  }
  .role {
    margin-left: 0.4rem;
    padding: 0.05rem 0.4rem;
    border-radius: 0.4rem;
    background: rgba(212, 175, 55, 0.18);
    color: #d4af37;
    font-size: 0.72rem;
    text-transform: uppercase;
  }
  .token-row {
    display: flex;
    gap: 0.5rem;
  }
  .token {
    width: 100%;
    margin-top: 0.5rem;
    font-family: ui-monospace, monospace;
    font-size: 0.75rem;
  }
  .ghost-button.danger {
    color: #e06b6b;
  }
</style>

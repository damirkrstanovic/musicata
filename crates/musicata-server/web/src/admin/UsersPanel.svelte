<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { api, ApiError, type SessionUser } from "../lib/api";
  import { session } from "../lib/session.svelte";
  import { confirmAction, promptText } from "../lib/modal";

  let users = $state<SessionUser[]>([]);
  let username = $state("");
  let password = $state("");
  let role = $state("listener");
  let message = $state("");
  let error = $state(false);
  let busy = $state(false);

  async function load() {
    try {
      users = (await api.listUsers()).users;
    } catch (e) {
      message = e instanceof ApiError ? e.message : String(e);
      error = true;
    }
  }
  load();

  function note(text: string, isError = false) {
    message = text;
    error = isError;
  }

  async function create(event: SubmitEvent) {
    event.preventDefault();
    busy = true;
    note("");
    try {
      await api.createUser(username.trim(), password, role);
      username = "";
      password = "";
      role = "listener";
      note("User added.");
      await load();
    } catch (e) {
      note(e instanceof ApiError ? e.message : String(e), true);
    } finally {
      busy = false;
    }
  }

  async function toggleRole(user: SessionUser) {
    const nextRole = user.role === "admin" ? "listener" : "admin";
    try {
      await api.updateUser(user.id, { role: nextRole });
      await load();
    } catch (e) {
      note(e instanceof ApiError ? e.message : String(e), true);
    }
  }

  async function resetPassword(user: SessionUser) {
    const pw = await promptText({
      title: `Reset ${user.username}'s password`,
      label: "New password (min 8 characters)",
    });
    if (!pw) return;
    try {
      await api.updateUser(user.id, { password: pw });
      note(`Reset ${user.username}'s password.`);
    } catch (e) {
      note(e instanceof ApiError ? e.message : String(e), true);
    }
  }

  async function remove(user: SessionUser) {
    if (
      !(await confirmAction({
        title: "Remove user",
        message: `Remove “${user.username}”?`,
        confirmLabel: "Remove",
      }))
    )
      return;
    try {
      await api.deleteUser(user.id);
      await load();
    } catch (e) {
      note(e instanceof ApiError ? e.message : String(e), true);
    }
  }
</script>

<section class="admin-panel">
  <div class="admin-panel-head"><h2>Users</h2></div>
  <p class="admin-hint">Accounts that can sign in. Admins manage settings, sources, and users.</p>

  <div class="admin-list">
    {#each users as user (user.id)}
      <div class="admin-row">
        <div class="admin-row-main">
          <div class="admin-row-title">
            <strong>{user.username}</strong>
            <span class="tag">{user.role}</span>
            {#if user.id === session.user?.id}<span class="tag you">you</span>{/if}
          </div>
        </div>
        <div class="user-actions">
          <button type="button" class="ghost-button" onclick={() => toggleRole(user)}>
            {user.role === "admin" ? "Make listener" : "Make admin"}
          </button>
          <button type="button" class="ghost-button" onclick={() => resetPassword(user)}>Reset password</button>
          {#if user.id !== session.user?.id}
            <button type="button" class="ghost-button danger" onclick={() => remove(user)}>Remove</button>
          {/if}
        </div>
      </div>
    {/each}
  </div>

  <form class="field-form" onsubmit={create}>
    <h3>Add a user</h3>
    <div class="field-grid">
      <label class="field"><span>Username</span><input bind:value={username} required /></label>
      <label class="field"><span>Password</span><input type="password" bind:value={password} required /></label>
      <label class="field">
        <span>Role</span>
        <select bind:value={role}>
          <option value="listener">Listener</option>
          <option value="admin">Admin</option>
        </select>
      </label>
    </div>
    <div class="field-actions">
      <button type="submit" class="primary-button" disabled={busy}>Add user</button>
      <span class="form-status" class:error>{message}</span>
    </div>
  </form>
</section>

<style>
  .user-actions {
    display: flex;
    flex-wrap: wrap;
    gap: 0.4rem;
  }
  .tag.you {
    background: rgba(212, 175, 55, 0.18);
    color: #d4af37;
  }
  .ghost-button.danger {
    color: #e06b6b;
  }
</style>

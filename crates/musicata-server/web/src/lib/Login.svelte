<script lang="ts">
  import { ApiError } from "./api";
  import { session } from "./session.svelte";

  // "setup" creates the first admin account; "login" signs in to an existing one.
  let { mode }: { mode: "setup" | "login" } = $props();

  let username = $state("");
  let password = $state("");
  let busy = $state(false);
  let error = $state("");

  const isSetup = $derived(mode === "setup");

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    busy = true;
    error = "";
    try {
      if (isSetup) {
        await session.setup(username.trim(), password);
      } else {
        await session.login(username.trim(), password);
      }
    } catch (e) {
      error = e instanceof ApiError ? e.message : String(e);
    } finally {
      busy = false;
    }
  }
</script>

<div class="login-shell">
  <form class="login-card" onsubmit={submit}>
    <h1>Musicata</h1>
    {#if isSetup}
      <p class="login-hint">Create the first account. It will be an administrator.</p>
    {:else}
      <p class="login-hint">Sign in to continue.</p>
    {/if}
    <label class="field">
      <span>Username</span>
      <input bind:value={username} autocomplete="username" required />
    </label>
    <label class="field">
      <span>Password</span>
      <input
        bind:value={password}
        type="password"
        autocomplete={isSetup ? "new-password" : "current-password"}
        required
      />
    </label>
    {#if error}<p class="login-error">{error}</p>{/if}
    <button type="submit" class="primary-button" disabled={busy}>
      {isSetup ? "Create account" : "Sign in"}
    </button>
  </form>
</div>

<style>
  .login-shell {
    min-height: 100vh;
    min-height: 100dvh;
    display: grid;
    place-items: center;
    padding: 1.5rem;
    background: #16181d;
    color: #e9ecf1;
  }
  .login-card {
    width: 100%;
    max-width: 22rem;
    display: flex;
    flex-direction: column;
    gap: 0.85rem;
    background: #20242c;
    border: 1px solid rgba(255, 255, 255, 0.08);
    border-radius: 0.9rem;
    padding: 1.6rem 1.4rem;
    box-shadow: 0 12px 40px rgba(0, 0, 0, 0.45);
  }
  .login-card h1 {
    margin: 0;
    font-size: 1.4rem;
    color: #d4af37;
  }
  .login-hint {
    margin: 0;
    font-size: 0.85rem;
    color: #aab1bd;
  }
  .field {
    display: flex;
    flex-direction: column;
    gap: 0.3rem;
    font-size: 0.8rem;
  }
  .field input {
    padding: 0.55rem 0.65rem;
    border-radius: 0.5rem;
    border: 1px solid rgba(255, 255, 255, 0.14);
    background: #16181d;
    color: inherit;
  }
  .login-error {
    margin: 0;
    color: #e06b6b;
    font-size: 0.82rem;
  }
  .primary-button {
    margin-top: 0.3rem;
    padding: 0.6rem;
    border: none;
    border-radius: 0.5rem;
    background: #d4af37;
    color: #16181d;
    font-weight: 600;
    cursor: pointer;
  }
  .primary-button:disabled {
    opacity: 0.6;
    cursor: default;
  }
</style>

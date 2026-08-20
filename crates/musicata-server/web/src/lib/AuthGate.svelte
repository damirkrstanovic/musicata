<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import type { Component } from "svelte";
  import { onMount } from "svelte";
  import { session } from "./session.svelte";
  import Login from "./Login.svelte";

  // `app` is the page's root component, mounted only once the user is authenticated.
  // `requireAdmin` gates a page (the /admin app) to administrators.
  let { app, requireAdmin = false }: { app: Component; requireAdmin?: boolean } = $props();
  const App = $derived(app);

  onMount(() => {
    session.init();
  });
</script>

{#if !session.ready}
  <div class="gate-blank" aria-busy="true"></div>
{:else if session.setupRequired}
  <Login mode="setup" />
{:else if !session.user}
  <Login mode="login" />
{:else if requireAdmin && !session.isAdmin}
  <div class="gate-blank gate-message">
    <p>Administrator access required.</p>
    <a href="/">Back to player</a>
  </div>
{:else}
  <App />
{/if}

<style>
  .gate-blank {
    min-height: 100vh;
    min-height: 100dvh;
    background: #16181d;
  }
  .gate-message {
    display: grid;
    place-items: center;
    gap: 0.5rem;
    color: #aab1bd;
    text-align: center;
  }
  .gate-message a {
    color: #d4af37;
  }
</style>

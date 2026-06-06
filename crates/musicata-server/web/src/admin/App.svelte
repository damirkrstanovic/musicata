<script lang="ts">
  // Phase 0 scaffold for the /admin entry. Real admin components land in Phase 2.
  let clicks = $state(0);
</script>

<main data-testid="svelte-shell">
  <h1>Musicata — Admin</h1>
  <p>Svelte admin shell (Phase 0 scaffold).</p>
  <button onclick={() => clicks++}>reactivity check: {clicks}</button>
</main>

<style>
  main {
    font-family: system-ui, sans-serif;
    padding: 2rem;
  }
</style>

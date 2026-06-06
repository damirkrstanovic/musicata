<script lang="ts">
  // Phase 0 scaffold — proves the Svelte 5 (runes) + Vite + embed pipeline end to end.
  // Real player components land in Phase 3.
  let clicks = $state(0);
</script>

<main data-testid="svelte-shell">
  <h1>Musicata</h1>
  <p>Svelte player shell (Phase 0 scaffold).</p>
  <button onclick={() => clicks++}>reactivity check: {clicks}</button>
</main>

<style>
  main {
    font-family: system-ui, sans-serif;
    padding: 2rem;
  }
</style>

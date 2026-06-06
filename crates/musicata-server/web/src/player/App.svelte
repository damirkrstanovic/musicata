<script lang="ts">
  // Phase 1 scaffold — exercises the typed API client (generated-from-Rust types) end to
  // end. Real player components land in Phase 3.
  import { api } from "../lib/api";
  import type { LibrarySummary } from "../types/LibrarySummary";

  let summary = $state<LibrarySummary | null>(null);
  let error = $state<string | null>(null);

  // Pure CSR (mount()), so the script runs in the browser — fetch at init is fine.
  api
    .librarySummary()
    .then((s) => (summary = s))
    .catch((e) => (error = String(e)));
</script>

<main data-testid="svelte-shell">
  <h1>Musicata</h1>
  {#if error}
    <p>Failed to load library: {error}</p>
  {:else if summary}
    <p>
      {summary.artist_count} artists · {summary.album_count} albums · {summary.track_count} tracks
    </p>
  {:else}
    <p>Loading…</p>
  {/if}
</main>

<style>
  main {
    font-family: system-ui, sans-serif;
    padding: 2rem;
  }
</style>

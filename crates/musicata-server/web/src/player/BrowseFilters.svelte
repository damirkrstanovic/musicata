<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { browse } from "../lib/browse.svelte";

  let genreSearch = $state("");
  const normalizedGenres = $derived(
    (browse.index?.genres ?? []).map((genre) => ({
      ...genre,
      display: genre.value.trim().replace(/\s+/g, " "),
    })),
  );
  const matchingGenres = $derived(
    normalizedGenres.filter((genre) => genre.display.toLocaleLowerCase().includes(genreSearch.trim().toLocaleLowerCase())),
  );

  // Lazy-load the facet index the first time the filters render.
  if (!browse.index) browse.load();
</script>

{#if browse.index}
  <div class="browse-filters">
    <label>
      <span class="sr-only">Search genres</span>
      <input type="search" bind:value={genreSearch} placeholder="Search genres" aria-label="Search genres" />
    </label>
    <select bind:value={browse.genre} aria-label="Genre">
      <option value="">All genres</option>
      {#each matchingGenres as g (g.value)}<option value={g.value}>{g.display} ({g.track_count})</option>{/each}
    </select>
    <select
      value={browse.year ?? ""}
      aria-label="Year"
      onchange={(e) => {
        const v = (e.currentTarget as HTMLSelectElement).value;
        browse.year = v ? Number(v) : null;
      }}
    >
      <option value="">All years</option>
      {#each browse.index.years as y (y.value)}<option value={y.value}>{y.value}</option>{/each}
    </select>
    <select bind:value={browse.composer} aria-label="Composer">
      <option value="">All composers</option>
      {#each browse.index.composers as c (c.value)}<option value={c.value}>{c.value}</option>{/each}
    </select>
    {#if browse.index.tags.length}
      <select bind:value={browse.tag} aria-label="Sound">
        <option value="">All sounds</option>
        {#each browse.index.tags as t (t.value)}<option value={t.value}>{t.value}</option>{/each}
      </select>
    {/if}
    {#if browse.active}
      <button class="ghost-button" type="button" onclick={() => browse.reset()}>Clear</button>
    {/if}
  </div>
{/if}

<style>
  .sr-only { position:absolute; width:1px; height:1px; padding:0; overflow:hidden; clip-path:inset(50%); white-space:nowrap; }
  label { min-width:0; width:100%; }
  input[type="search"] { width:100%; min-width:0; min-height:44px; padding:8px 10px; border:1px solid var(--line); border-radius:8px; color:var(--text); background:var(--panel-strong); font-size:16px; }
</style>

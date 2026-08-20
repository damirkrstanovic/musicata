<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { browse } from "../lib/browse.svelte";

  // Lazy-load the facet index the first time the filters render.
  if (!browse.index) browse.load();
</script>

{#if browse.index}
  <div class="browse-filters">
    <select bind:value={browse.genre} aria-label="Genre">
      <option value="">All genres</option>
      {#each browse.index.genres as g (g.value)}<option value={g.value}>{g.value}</option>{/each}
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

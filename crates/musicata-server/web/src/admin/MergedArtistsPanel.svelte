<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { api, ApiError, type ArtistAliasGroup } from "../lib/api";
  import type { Artist } from "../types/Artist";
  import { confirmAction } from "../lib/modal";

  let aliases = $state<ArtistAliasGroup[]>([]);
  let allArtists = $state<Artist[]>([]);
  let query = $state("");
  let selected = $state<Map<string, string>>(new Map());
  let canonical = $state<string | null>(null);
  let status = $state("");
  let error = $state(false);

  async function loadAliases() {
    try {
      aliases = await api.aliases();
    } catch {
      aliases = [];
    }
  }
  async function loadArtists() {
    try {
      allArtists = (await api.artists({ limit: 10000 })).items;
    } catch {
      allArtists = [];
    }
  }
  loadAliases();
  loadArtists();

  // Up to 8 matches, excluding already-selected; only when 2+ chars typed.
  const matches = $derived(
    query.trim().length < 2
      ? []
      : allArtists
          .filter(
            (a) =>
              !selected.has(a.id) && a.name.toLowerCase().includes(query.trim().toLowerCase()),
          )
          .slice(0, 8),
  );

  function add(artist: Artist) {
    const next = new Map(selected);
    next.set(artist.id, artist.name);
    selected = next;
    if (canonical == null) canonical = artist.id;
    query = "";
  }
  function remove(id: string) {
    const next = new Map(selected);
    next.delete(id);
    selected = next;
    if (canonical === id) canonical = next.keys().next().value ?? null;
  }

  async function merge() {
    if (selected.size < 2 || canonical == null) return;
    status = "Merging…";
    error = false;
    try {
      await api.mergeArtists({
        canonical_id: canonical,
        member_ids: [...selected.keys()].filter((id) => id !== canonical),
      });
      selected = new Map();
      canonical = null;
      status = "Merged.";
      await loadAliases();
    } catch (e) {
      status = e instanceof ApiError ? e.message : String(e);
      error = true;
    }
  }

  async function unmerge(key: string) {
    if (!(await confirmAction({ title: "Unmerge this artist?" }))) return;
    try {
      await api.unmergeArtist(key);
      await loadAliases();
    } catch (e) {
      status = e instanceof ApiError ? e.message : String(e);
      error = true;
    }
  }
</script>

<section class="admin-panel">
  <div class="admin-panel-head"><h2>Merged artists</h2></div>
  <p class="admin-hint">Group name variants (e.g. “Beatles” and “The Beatles”) under one artist.</p>

  <div class="admin-list">
    {#each aliases as group (group.canonical_key)}
      <div class="admin-row">
        <div class="admin-row-main">
          <div class="admin-row-title"><strong>{group.canonical_name}</strong></div>
          <div class="chips">
            {#each group.members as member (member)}
              <span class="chip">
                {member}
                <button type="button" class="merge-unmerge" title="Unmerge" onclick={() => unmerge(member)}>✕</button>
              </span>
            {/each}
          </div>
        </div>
      </div>
    {/each}
  </div>

  <div class="field-form">
    <h3>Merge artists</h3>
    {#if selected.size}
      <div class="merge-selected">
        {#each [...selected] as [id, name] (id)}
          <span class="merge-chip" class:is-canonical={canonical === id}>
            <button type="button" class="merge-canon" title="Make canonical" onclick={() => (canonical = id)}>
              {canonical === id ? "★" : "☆"}
            </button>
            {name}
            <button type="button" class="merge-remove" title="Remove" onclick={() => remove(id)}>✕</button>
          </span>
        {/each}
      </div>
    {/if}
    <label class="field">
      <span>Find an artist</span>
      <input bind:value={query} placeholder="Type a name…" />
    </label>
    {#if matches.length}
      <div class="merge-results">
        {#each matches as artist (artist.id)}
          <button type="button" class="merge-result" onclick={() => add(artist)}>{artist.name}</button>
        {/each}
      </div>
    {/if}
    <div class="field-actions">
      <button type="button" class="primary-button" disabled={selected.size < 2} onclick={merge}>
        Merge {selected.size} artists
      </button>
      <span class="form-status" class:error>{status}</span>
    </div>
  </div>
</section>

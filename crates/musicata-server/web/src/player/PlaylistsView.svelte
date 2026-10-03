<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { api, type Playlist, type SmartPlaylist } from "../lib/api";
  import { nav } from "../lib/nav.svelte";
  import { promptText } from "../lib/modal";

  let playlists = $state<Playlist[]>([]);
  let smart = $state<SmartPlaylist[]>([]);
  let loading = $state(true);
  let failed = $state(false);
  let creating = $state(false);

  async function load() {
    failed = false;
    loading = true;
    try { [playlists, smart] = await Promise.all([api.playlists(), api.smartPlaylists()]); }
    catch { failed = true; }
    finally { loading = false; }
  }
  void load();

  async function create() {
    const name = await promptText({title: "New playlist", label: "Name", confirmLabel: "Create"});
    if (!name) return;
    creating = true;
    try {
      const playlist = await api.createPlaylist(name);
      if (playlist) nav.push({name: "playlist", id: playlist.id, label: playlist.name});
    } finally { creating = false; }
  }
</script>

<section class="saved-playlists" aria-label="Saved playlists">
  <button class="ghost-button" type="button" disabled={creating} onclick={create}>＋ New playlist</button>
  {#if loading}
    <p class="admin-hint">Loading playlists…</p>
  {:else if failed}
    <p class="admin-hint">Couldn't load playlists. <button class="ghost-button" type="button" onclick={load}>Retry</button></p>
  {:else}
    {#if !playlists.length && !smart.length}<p class="admin-hint">Create a playlist to save music for later.</p>{/if}
    {#each playlists as p (p.id)}
      <button class="saved-playlist" type="button" onclick={() => nav.push({name: "playlist", id: p.id, label: p.name})}>
        <span aria-hidden="true">♫</span><strong>{p.name}</strong><span aria-hidden="true">›</span>
      </button>
    {/each}
    {#if smart.length}<h3>Smart playlists</h3>{/if}
    {#each smart as p (p.id)}
      <button class="saved-playlist" type="button" onclick={() => nav.push({name: "smart", id: p.id, label: p.name})}>
        <span aria-hidden="true">✧</span><strong>{p.name}</strong><span aria-hidden="true">›</span>
      </button>
    {/each}
  {/if}
</section>

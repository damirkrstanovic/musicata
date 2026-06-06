<script lang="ts">
  import { api, type Playlist, type SmartPlaylist } from "../lib/api";
  import { nav } from "../lib/nav.svelte";
  import { promptText } from "../lib/modal";

  let playlists = $state<Playlist[]>([]);
  let smart = $state<SmartPlaylist[]>([]);

  async function load() {
    try {
      [playlists, smart] = await Promise.all([api.playlists(), api.smartPlaylists()]);
    } catch {
      // leave previous
    }
  }
  load();

  async function create() {
    const name = await promptText({ title: "New playlist", label: "Name", confirmLabel: "Create" });
    if (!name) return;
    await api.createPlaylist(name);
    await load();
  }
</script>

<div class="admin-panel-head">
  <h2>Playlists</h2>
  <button class="ghost-button" type="button" onclick={create}>New playlist</button>
</div>
<div class="admin-list">
  {#each playlists as p (p.id)}
    <div class="admin-row">
      <button class="admin-row-main" type="button" onclick={() => nav.push({ name: "playlist", id: p.id, label: p.name })}>
        <div class="admin-row-title"><strong>{p.name}</strong></div>
        <span class="ident-label">{p.song_count} tracks</span>
      </button>
    </div>
  {:else}
    <p class="admin-hint">No playlists yet.</p>
  {/each}
</div>

<h2 style="margin-top:1.5rem">Smart playlists</h2>
<div class="admin-list">
  {#each smart as s (s.id)}
    <div class="admin-row">
      <button class="admin-row-main" type="button" onclick={() => nav.push({ name: "smart", id: s.id, label: s.name })}>
        <div class="admin-row-title"><strong>{s.name}</strong></div>
        <span class="ident-label">{s.description}</span>
      </button>
    </div>
  {/each}
</div>

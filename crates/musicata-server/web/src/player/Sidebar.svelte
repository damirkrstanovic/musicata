<script lang="ts">
  import { api, type Playlist, type SmartPlaylist } from "../lib/api";
  import type { RadioStation } from "../types/RadioStation";
  import { nav } from "../lib/nav.svelte";
  import { search } from "../lib/search.svelte";
  import { promptText } from "../lib/modal";
  import { playStream } from "../lib/playback";
  import BrowseFilters from "./BrowseFilters.svelte";

  let playlists = $state<Playlist[]>([]);
  let smart = $state<SmartPlaylist[]>([]);
  let stations = $state<RadioStation[]>([]);

  async function load() {
    try {
      [playlists, smart, stations] = await Promise.all([
        api.playlists(),
        api.smartPlaylists(),
        api.radio(),
      ]);
    } catch {
      // keep previous
    }
  }
  load();

  let searchTimer: ReturnType<typeof setTimeout> | undefined;
  function onSearch(value: string) {
    clearTimeout(searchTimer);
    searchTimer = setTimeout(() => {
      search.query = value;
      if (value.trim()) {
        if (nav.current.name !== "search") nav.root({ name: "search" });
      } else if (nav.current.name === "search") {
        nav.root({ name: "library" });
      }
    }, 220);
  }

  async function newPlaylist() {
    const name = await promptText({ title: "New playlist", label: "Name", confirmLabel: "Create" });
    if (!name) return;
    await api.createPlaylist(name);
    await load();
  }
</script>

<aside class="library-panel">
  <div class="brand">
    <span class="brand-mark">M</span>
    <div class="brand-text"><h1>Musicata</h1><p>Player</p></div>
  </div>

  <label class="search">
    <input type="search" autocomplete="off" placeholder="Search" oninput={(e) => onSearch(e.currentTarget.value)} />
  </label>

  <nav class="library-nav" aria-label="Views">
    <button
      class="nav-link"
      class:is-active={nav.current.name === "favorites"}
      type="button"
      onclick={() => nav.root({ name: "favorites" })}>Favorites</button
    >
  </nav>

  <section class="section">
    <h2>Browse</h2>
    <BrowseFilters />
  </section>

  <section class="section">
    <div class="section-head">
      <h2>Playlists</h2>
      <button class="ghost-button mini" type="button" title="New playlist" onclick={newPlaylist}>＋</button>
    </div>
    <div class="playlist-list">
      {#each playlists as p (p.id)}
        <button
          class="nav-link"
          class:is-active={nav.current.name === "playlist" && nav.current.id === p.id}
          type="button"
          onclick={() => nav.push({ name: "playlist", id: p.id, label: p.name })}>{p.name}</button
        >
      {/each}
    </div>
  </section>

  <section class="section">
    <div class="section-head"><h2>Smart playlists</h2></div>
    <div class="playlist-list">
      {#each smart as s (s.id)}
        <button
          class="nav-link"
          class:is-active={nav.current.name === "smart" && nav.current.id === s.id}
          type="button"
          onclick={() => nav.push({ name: "smart", id: s.id, label: s.name })}>{s.name}</button
        >
      {/each}
    </div>
  </section>

  <section class="section">
    <div class="section-head"><h2>Radio</h2></div>
    <div class="playlist-list">
      {#each stations as st (st.id)}
        <button class="nav-link" type="button" onclick={() => playStream(st.stream_url, st.name)}>{st.name}</button>
      {/each}
    </div>
  </section>
</aside>

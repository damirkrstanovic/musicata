<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { onDestroy } from "svelte";
  import { api, type Playlist, type SmartPlaylist } from "../lib/api";
  import type { RadioStation } from "../types/RadioStation";
  import { nav } from "../lib/nav.svelte";
  import { search } from "../lib/search.svelte";
  import { promptText } from "../lib/modal";
  import { playStream } from "../lib/playback";
  import { radioMix } from "../lib/radioMix.svelte";
  import BrowseFilters from "./BrowseFilters.svelte";
  import AccountMenu from "./AccountMenu.svelte";

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

  // Search is a live filter on the current Tracks/Albums/Artists segment (not a separate
  // view), so it persists when you switch segments. The grid views react to search.query.
  let searchTimer: ReturnType<typeof setTimeout> | undefined;
  function onSearch(value: string) {
    clearTimeout(searchTimer);
    searchTimer = setTimeout(() => (search.query = value), 220);
  }
  onDestroy(() => clearTimeout(searchTimer));

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
    {#if radioMix.active}
      <button
        class="nav-link"
        class:is-active={nav.current.name === "mix"}
        type="button"
        onclick={() => nav.push({ name: "mix" })}>Mix</button
      >
    {/if}
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
        <!-- Play the station through the server, not its upstream URL directly: the CSP
             restricts media to this origin, and the relay keeps the user's IP off the
             station's logs. `st.stream_url` remains the real URL, for editing in /admin. -->
        <button class="nav-link" type="button" onclick={() => playStream(`/api/radio/${encodeURIComponent(st.id)}/stream`, st.name)}>{st.name}</button>
      {/each}
    </div>
  </section>

  <AccountMenu />
</aside>

<script lang="ts">
  import { onMount, onDestroy } from "svelte";
  import { api } from "../lib/api";
  import { player } from "../lib/player.svelte";
  import { nav } from "../lib/nav.svelte";
  import { connectPlayer, type PlayerSocket, type ProgressTick } from "../lib/playerWs";
  import { BrowserAudio } from "../lib/audio";
  import { setAudio } from "../lib/playback";
  import { sendCommand } from "../lib/commands";
  import { setMediaMetadata, setMediaPosition, setMediaHandlers } from "../lib/media";
  import { search } from "../lib/search.svelte";
  import { favorites } from "../lib/favorites.svelte";
  import type { PlaybackState } from "../types/PlaybackState";
  import Modal from "../lib/Modal.svelte";
  import Footer from "./Footer.svelte";
  import LibraryGrid from "./LibraryGrid.svelte";
  import ArtistsGrid from "./ArtistsGrid.svelte";
  import AlbumDetail from "./AlbumDetail.svelte";
  import ArtistDetail from "./ArtistDetail.svelte";
  import SearchResults from "./SearchResults.svelte";
  import QueueDrawer from "./QueueDrawer.svelte";
  import FavoritesView from "./FavoritesView.svelte";
  import PlaylistsIndex from "./PlaylistsIndex.svelte";
  import PlaylistView from "./PlaylistView.svelte";
  import SmartPlaylistView from "./SmartPlaylistView.svelte";
  import RadioView from "./RadioView.svelte";

  let audioEl: HTMLAudioElement;
  let audio: BrowserAudio | null = null;
  let ws: PlayerSocket | null = null;

  // A value (not a getter call) so TS narrows `route` in each branch below.
  const route = $derived(nav.current);

  // Hot path: a tick moves only elapsed/duration (+ the OS scrubber).
  function applyTick(tick: ProgressTick) {
    player.elapsed = tick.elapsed_seconds ?? 0;
    if (tick.duration_seconds != null) player.duration = tick.duration_seconds;
    setMediaPosition(player.elapsed, player.duration);
  }

  function applyState(next: PlaybackState) {
    const trackChanged =
      (player.playback?.now_playing?.track_id ?? null) !== (next.now_playing?.track_id ?? null);
    const statusChanged = player.playback?.status !== next.status;
    player.playback = next;
    if (!player.seekDragging) {
      player.elapsed = next.elapsed_seconds ?? 0;
      player.duration = next.duration_seconds ?? 0;
    }
    audio?.drive(next);
    if (trackChanged || statusChanged) setMediaMetadata(next.now_playing, next.status);
    setMediaPosition(player.elapsed, player.duration);
  }

  onMount(async () => {
    audio = new BrowserAudio(audioEl);
    setAudio(audio);
    audio.onProgress((msg) => ws?.send(msg));
    audio.onEnded(() => ws?.send({ type: "ended" }));
    audio.start();

    setMediaHandlers({
      play: () => sendCommand(player.activeId, { command: "play" }),
      pause: () => sendCommand(player.activeId, { command: "pause" }),
      previoustrack: () => sendCommand(player.activeId, { command: "previous" }),
      nexttrack: () => sendCommand(player.activeId, { command: "next" }),
      seekto: (d) => {
        if (d.seekTime != null)
          sendCommand(player.activeId, { command: "seek", position_seconds: d.seekTime });
      },
    });

    favorites.load();
    const players = await api.players();
    const browser = players.find((p) => p.kind === "browser") ?? players[0];
    if (!browser) return;
    player.activeId = browser.id;
    ws = connectPlayer(browser.id, { onState: applyState, onProgress: applyTick });
  });

  onDestroy(() => {
    ws?.close();
    audio?.stop();
  });

  // Debounce search input; switch to/from the search view as the box gains/loses text.
  let searchTimer: ReturnType<typeof setTimeout> | undefined;
  function onSearchInput(value: string) {
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
</script>

<svelte:window onpopstate={() => nav.pop()} />

<div class="player-shell">
  <header class="app-bar">
    {#if nav.canGoBack}
      <button class="ghost-button" type="button" onclick={() => nav.pop()}>← Back</button>
    {/if}
    <nav class="app-tabs">
      <button
        class="tab"
        class:active={route.name === "library"}
        type="button"
        onclick={() => nav.root({ name: "library" })}>Albums</button
      >
      <button
        class="tab"
        class:active={route.name === "artists"}
        type="button"
        onclick={() => nav.root({ name: "artists" })}>Artists</button
      >
      <button
        class="tab"
        class:active={route.name === "favorites"}
        type="button"
        onclick={() => nav.root({ name: "favorites" })}>Favorites</button
      >
      <button
        class="tab"
        class:active={route.name === "playlists"}
        type="button"
        onclick={() => nav.root({ name: "playlists" })}>Playlists</button
      >
      <button
        class="tab"
        class:active={route.name === "radio"}
        type="button"
        onclick={() => nav.root({ name: "radio" })}>Radio</button
      >
    </nav>
    <label class="search">
      <input type="search" autocomplete="off" placeholder="Search" oninput={(e) => onSearchInput(e.currentTarget.value)} />
    </label>
    <a class="ghost-button" href="/v2/admin">Admin</a>
  </header>

  <main class="player-main">
    {#if route.name === "library"}
      <LibraryGrid />
    {:else if route.name === "artists"}
      <ArtistsGrid />
    {:else if route.name === "favorites"}
      <FavoritesView />
    {:else if route.name === "playlists"}
      <PlaylistsIndex />
    {:else if route.name === "radio"}
      <RadioView />
    {:else if route.name === "search"}
      <SearchResults />
    {:else if route.name === "album"}
      <AlbumDetail id={route.id} />
    {:else if route.name === "artist"}
      <ArtistDetail id={route.id} />
    {:else if route.name === "playlist"}
      <PlaylistView id={route.id} />
    {:else if route.name === "smart"}
      <SmartPlaylistView id={route.id} />
    {/if}
  </main>

  <Footer />
  <QueueDrawer />
  <Modal />
  <audio bind:this={audioEl} preload="none" hidden></audio>
</div>

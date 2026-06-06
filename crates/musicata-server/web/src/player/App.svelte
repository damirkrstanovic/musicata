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
  import type { Player } from "../types/Player";
  import type { Zone } from "../types/Zone";
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
    if (player.isBrowserOutput) audio?.drive(next);
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
      play: () => sendCommand(player.target, { command: "play" }),
      pause: () => sendCommand(player.target, { command: "pause" }),
      previoustrack: () => sendCommand(player.target, { command: "previous" }),
      nexttrack: () => sendCommand(player.target, { command: "next" }),
      seekto: (d) => {
        if (d.seekTime != null)
          sendCommand(player.target, { command: "seek", position_seconds: d.seekTime });
      },
    });

    favorites.load();
    await loadTargets();
    const browser = players.find((p) => p.kind === "browser") ?? players[0];
    if (browser) connect("player", browser.id);
  });

  // Players + zones for the output switcher.
  let players = $state<Player[]>([]);
  let zones = $state<Zone[]>([]);
  async function loadTargets() {
    try {
      const [ps, zs] = await Promise.all([api.players(), api.zones()]);
      players = ps;
      zones = zs;
      const browser = ps.find((p) => p.kind === "browser");
      player.browserId = browser?.id ?? null;
      player.browserZoneId = browser?.zone_id ?? null;
    } catch {
      // keep previous
    }
  }

  function connect(kind: "player" | "zone", id: string) {
    ws?.close();
    player.activeKind = kind;
    player.activeId = id;
    player.playback = null;
    player.elapsed = 0;
    player.duration = 0;
    ws = connectPlayer(kind, id, { onState: applyState, onProgress: applyTick });
  }

  function onTargetChange(value: string) {
    const [kind, id] = value.split(":") as ["player" | "zone", string];
    connect(kind, id);
  }

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
    <select
      class="target-select"
      aria-label="Output"
      value={`${player.activeKind}:${player.activeId}`}
      onchange={(e) => onTargetChange(e.currentTarget.value)}
    >
      {#each players as p (p.id)}
        <option value={`player:${p.id}`}>{p.name}</option>
      {/each}
      {#each zones as z (z.id)}
        <option value={`zone:${z.id}`}>Zone · {z.name}</option>
      {/each}
    </select>
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

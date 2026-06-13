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
  import { favorites } from "../lib/favorites.svelte";
  import { search } from "../lib/search.svelte";
  import { dsp } from "../lib/dsp.svelte";
  import { autoplay } from "../lib/autoplay.svelte";
  import type { PlaybackState } from "../types/PlaybackState";
  import type { Player } from "../types/Player";
  import type { Zone } from "../types/Zone";
  import Modal from "../lib/Modal.svelte";
  import Sidebar from "./Sidebar.svelte";
  import Footer from "./Footer.svelte";
  import TracksView from "./TracksView.svelte";
  import LibraryGrid from "./LibraryGrid.svelte";
  import ArtistsGrid from "./ArtistsGrid.svelte";
  import AlbumDetail from "./AlbumDetail.svelte";
  import ArtistDetail from "./ArtistDetail.svelte";
  import QueueDrawer from "./QueueDrawer.svelte";
  import EqPanel from "./EqPanel.svelte";
  import VuMeter from "./VuMeter.svelte";
  import MetadataPanel from "./MetadataPanel.svelte";
  import FavoritesView from "./FavoritesView.svelte";
  import PlaylistView from "./PlaylistView.svelte";
  import SmartPlaylistView from "./SmartPlaylistView.svelte";
  import InstallPrompt from "./InstallPrompt.svelte";
  import AccountMenu from "./AccountMenu.svelte";
  import { install } from "../lib/install.svelte";

  let audioEl: HTMLAudioElement;
  let audio: BrowserAudio | null = null;
  let ws: PlayerSocket | null = null;

  // A value (not a getter call) so TS narrows `route` in each branch below.
  const route = $derived(nav.current);

  // Center-panel title for the current route.
  const isSegment = $derived(
    route.name === "tracks" || route.name === "library" || route.name === "artists",
  );
  const title = $derived(
    search.query.trim() && isSegment
      ? `Search: ${search.query.trim()}`
      : route.name === "tracks"
        ? "Tracks"
        : route.name === "library"
          ? "Albums"
          : route.name === "artists"
            ? "Artists"
            : route.name === "favorites"
              ? "Favorites"
              : route.name === "album"
                ? route.title
                : route.name === "artist" || route.name === "playlist" || route.name === "smart"
                  ? route.label
                  : "Musicata",
  );

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
    if (trackChanged) {
      const np = next.now_playing;
      audio?.setTrackLoudness(np?.integrated_loudness_lufs ?? null, np?.true_peak_dbtp ?? null);
    }
    if (trackChanged || statusChanged) setMediaMetadata(next.now_playing, next.status);
    setMediaPosition(player.elapsed, player.duration);
  }

  // Push the active EQ profile into the Web Audio graph whenever it changes. IMPORTANT: read
  // the reactive dsp state into `profile` FIRST. If we inlined it as `audio?.setEq(dsp...)`,
  // the optional chain would short-circuit argument evaluation while `audio` is still null on
  // the first run, so the effect would track no dependencies and never re-run.
  $effect(() => {
    const profile = dsp.enabled ? dsp.active : null;
    audio?.setEq(profile);
  });

  // Volume leveling toggle → graph (read the reactive dep first; see the note above).
  $effect(() => {
    const leveling = dsp.leveling;
    audio?.setLeveling(leveling);
  });

  onMount(async () => {
    audio = new BrowserAudio(audioEl);
    setAudio(audio);
    (window as unknown as { __audio?: unknown }).__audio = audio; // debug hook
    audio.setEq(dsp.enabled ? dsp.active : null); // apply persisted profile on load
    audio.setLeveling(dsp.leveling);
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
    autoplay.load();
    dsp.load();
    install.init();
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
    ws = connectPlayer(kind, id, {
      onState: applyState,
      onProgress: applyTick,
      onDisconnect: () => {
        // Server went away — don't let buffered audio keep playing on this tab.
        if (player.isBrowserOutput) audio?.pause();
      },
    });
  }

  function onTargetChange(value: string) {
    const [kind, id] = value.split(":") as ["player" | "zone", string];
    connect(kind, id);
  }

  onDestroy(() => {
    ws?.close();
    audio?.stop();
  });
</script>

<svelte:window onpopstate={() => nav.pop()} />

<main class="shell">
  <Sidebar />

  <section class="content">
    <header class="content-header">
      {#if nav.canGoBack}
        <button class="back-btn" type="button" onclick={() => nav.pop()}>‹ Back</button>
      {/if}
      <div class="content-title">
        <p class="eyebrow">Library</p>
        <h2>{title}</h2>
      </div>
      <div class="content-controls">
        <div class="segmented" role="tablist" aria-label="Browse">
          <button
            class="seg"
            class:is-active={route.name === "tracks"}
            type="button"
            onclick={() => nav.root({ name: "tracks" })}>Tracks</button
          >
          <button
            class="seg"
            class:is-active={route.name === "library"}
            type="button"
            onclick={() => nav.root({ name: "library" })}>Albums</button
          >
          <button
            class="seg"
            class:is-active={route.name === "artists"}
            type="button"
            onclick={() => nav.root({ name: "artists" })}>Artists</button
          >
        </div>
        <a class="ghost-button" href="/admin">Admin</a>
      </div>
    </header>

    {#if route.name === "tracks"}
      <TracksView />
    {:else if route.name === "library"}
      <LibraryGrid />
    {:else if route.name === "artists"}
      <ArtistsGrid />
    {:else if route.name === "favorites"}
      <FavoritesView />
    {:else if route.name === "album"}
      <AlbumDetail id={route.id} />
    {:else if route.name === "artist"}
      <ArtistDetail id={route.id} />
    {:else if route.name === "playlist"}
      <PlaylistView id={route.id} />
    {:else if route.name === "smart"}
      <SmartPlaylistView id={route.id} />
    {/if}
  </section>

  <aside class="right-rail">
    <div class="rail-header">
      <div class="player-switch">
        <select
          class="player-switch-btn"
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
        <AccountMenu />
      </div>
    </div>
    <div class="rail-top">
      <MetadataPanel />
      <QueueDrawer />
      <EqPanel />
      <VuMeter />
    </div>
    <Footer />
  </aside>
</main>

<Modal />
<InstallPrompt />
<audio bind:this={audioEl} preload="none" hidden></audio>

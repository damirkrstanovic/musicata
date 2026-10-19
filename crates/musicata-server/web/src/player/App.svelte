<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { onMount, onDestroy, tick } from "svelte";
  import { api } from "../lib/api";
  import { player } from "../lib/player.svelte";
  import { nav, activityFor } from "../lib/nav.svelte";
  import { session } from "../lib/session.svelte";
  import SettingsWorkspace from "../admin/App.svelte";
  import { connectPlayer, type PlayerSocket, type ProgressTick } from "../lib/playerWs";
  import { BrowserAudio } from "../lib/audio";
  import { setAudio, resume, pause, next as skipNext, previous as skipPrevious } from "../lib/playback";
  import { sendCommand } from "../lib/commands";
  import { setMediaMetadata, setMediaPosition, setMediaHandlers } from "../lib/media";
  import { metadata } from "../lib/metadata.svelte";
  import { meter } from "../lib/meter.svelte";
  import { statsPanel } from "../lib/statsPanel.svelte";
  import { favorites } from "../lib/favorites.svelte";
  import { search } from "../lib/search.svelte";
  import { dsp } from "../lib/dsp.svelte";
  import { outputAudio, connectBrowserRenderer } from "../lib/outputAudio.svelte";
  import { autoplay } from "../lib/autoplay.svelte";
  import type { PlaybackState } from "../types/PlaybackState";
  import type { Player } from "../types/Player";
  import type { Zone } from "../types/Zone";
  import Modal from "../lib/Modal.svelte";
  import { activeModal } from "../lib/modal";
  import Sidebar from "./Sidebar.svelte";
  import Footer from "./Footer.svelte";
  import OutputPicker from "./OutputPicker.svelte";
  import NowPlaying from "./NowPlaying.svelte";
  import TracksView from "./TracksView.svelte";
  import LibraryGrid from "./LibraryGrid.svelte";
  import ArtistsGrid from "./ArtistsGrid.svelte";
  import AlbumDetail from "./AlbumDetail.svelte";
  import ArtistDetail from "./ArtistDetail.svelte";
  import QueueDrawer from "./QueueDrawer.svelte";
  import EqPanel from "./EqPanel.svelte";
  import VuMeter from "./VuMeter.svelte";
  import StatsPanel from "./StatsPanel.svelte";
  import MetadataPanel from "./MetadataPanel.svelte";
  import FavoritesView from "./FavoritesView.svelte";
  import PlaylistView from "./PlaylistView.svelte";
  import PlaylistsView from "./PlaylistsView.svelte";
  import SmartPlaylistView from "./SmartPlaylistView.svelte";
  import MixView from "./MixView.svelte";
  import RadioView from "./RadioView.svelte";
  import InstallPrompt from "./InstallPrompt.svelte";
  import { install } from "../lib/install.svelte";
  import { audioDevices } from "../lib/audioDevices.svelte";

  let audioEl: HTMLAudioElement;
  let audio: BrowserAudio | null = null;
  let ws: PlayerSocket | null = null;
  const navOpen = $derived(nav.overlays.includes("navigation"));
  const nowPlayingOpen = $derived(nav.overlays.includes("nowPlaying"));
  let mobile = $state(false);
  const auxiliaryOpen = $derived(dsp.panelOpen || metadata.trackId !== null || meter.open || statsPanel.open);
  const panelOpen = $derived(nowPlayingOpen || player.queueOpen || auxiliaryOpen);

  onMount(() => {
    const query = matchMedia("(max-width: 1023px)");
    const update = () => { mobile = query.matches; };
    update();
    nav.start();
    query.addEventListener("change", update);
    return () => { query.removeEventListener("change", update); nav.stop(); };
  });

  function closeAuxiliaryPanels() {
    dsp.panelOpen = false;
    metadata.close();
    if (meter.open) meter.toggle();
    statsPanel.open = false;
  }

  function toggleQueue() {
    const open = !player.queueOpen || auxiliaryOpen;
    closeAuxiliaryPanels();
    player.queueOpen = open;
  }

  async function setNowPlaying(open: boolean) {
    if (open) { closeAuxiliaryPanels(); nav.openOverlay("nowPlaying"); }
    else nav.closeOverlay();
    await tick();
    if (open) document.querySelector<HTMLButtonElement>('[aria-label="Close Now Playing"]')?.focus();
  }

  async function openNavigation() {
    nav.openOverlay("navigation");
    await tick();
    document.querySelector<HTMLButtonElement>('[aria-label="Close navigation"]')?.focus();
  }

  async function closeNavigation() {
    if (!navOpen) return;
    nav.closeOverlay();
    await tick();
    document.querySelector<HTMLButtonElement>('[aria-label="Open navigation"]')?.focus();
  }

  // Set when the connection drops while this tab was the playing output, so we resume on
  // reconnect: a restarted server restores the session as *paused* (it can't know a tab is still
  // here), which would otherwise leave the footer paused while audio was mid-track.
  let resumeOnReconnect = false;

  // A value (not a getter call) so TS narrows `route` in each branch below.
  const route = $derived(nav.current);
  const activity = $derived(activityFor(route));
  const listenTabs = [
    {name: "queue", label: "Queue"}, {name: "playlists", label: "Playlists"},
    {name: "mix", label: "Mixes"}, {name: "radio", label: "Stations"},
  ] as const;

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
            : route.name === "radio"
              ? "Stations"
            : route.name === "queue"
              ? "Queue"
            : route.name === "mix"
              ? "Mixes"
            : route.name === "settings"
              ? "Settings"
            : route.name === "favorites"
              ? "Favorites"
              : route.name === "playlists"
                ? "Playlists"
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
    player.connection = "online"; // a snapshot arrived → the link is live
    const trackChanged =
      (player.playback?.now_playing?.track_id ?? null) !== (next.now_playing?.track_id ?? null);
    const statusChanged = player.playback?.status !== next.status;
    player.playback = next;
    if (!player.seekDragging) {
      player.elapsed = next.elapsed_seconds ?? 0;
      player.duration = next.duration_seconds ?? 0;
    }
    // We were playing when the server went away, and it came back with the session paused (its
    // restore can't know this tab is still here) — resume so playback continues seamlessly.
    if (resumeOnReconnect && player.isBrowserOutput) {
      resumeOnReconnect = false;
      if (next.now_playing && next.status !== "playing") {
        void resume();
      }
    }
    if (player.isBrowserOutput) audio?.drive(next);
    if (trackChanged) {
      const np = next.now_playing;
      audio?.setTrackLoudness(
        np?.integrated_loudness_lufs ?? null,
        np?.true_peak_dbtp ?? null,
        np?.album_integrated_loudness_lufs ?? null,
        np?.album_true_peak_dbtp ?? null,
      );
    }
    if (trackChanged || statusChanged) setMediaMetadata(next.now_playing, next.status);
    setMediaPosition(player.elapsed, player.duration);
  }

  // Volume leveling toggle → graph (read the reactive dep first; see the note above).
  $effect(() => {
    const mode = dsp.levelingMode;
    audio?.setLevelingMode(mode);
  });

  // Tell the driver when this tab is the active browser output, so the first user gesture adopts
  // output and any transport control can drive audio (read the reactive dep first; see the note).
  $effect(() => {
    const isOutput = player.isBrowserOutput;
    audio?.setDesignatedOutput(isOutput);
  });

  onMount(async () => {
    audio = new BrowserAudio(audioEl);
    setAudio(audio);
    (window as unknown as { __audio?: unknown }).__audio = audio; // debug hook
    audio.setLevelingMode(dsp.levelingMode);
    audio.onProgress((msg) => { if (player.isBrowserOutput) ws?.send(msg); });
    audio.onEnded(() => { if (player.isBrowserOutput) ws?.send({ type: "ended" }); });
    audio.onBlocked((b) => (player.playBlocked = b));
    audio.start();

    setMediaHandlers({
      play: () => void resume(),
      pause: () => pause(),
      previoustrack: () => skipPrevious(),
      nexttrack: () => skipNext(),
      seekto: (d) => {
        if (d.seekTime != null)
          sendCommand(player.target, { command: "seek", position_seconds: d.seekTime });
      },
    });

    favorites.load();
    autoplay.load();
    install.init();
    audioDevices.init();
    await initConnection();
    // Restore the active output's EQ profile + sink (not its volume — don't override the
    // restored playback level just from booting). Wait for profiles to load first.
    await dsp.load();

  });

  // Players + zones for the output switcher.
  let players = $state<Player[]>([]);
  let zones = $state<Zone[]>([]);
  let zoneMemberId = $state<string | null>(null);

  // Connection resilience. Once `connect()` runs, connectPlayer reconnects the WS forever, so
  // the only gap is the *initial* target fetch failing (server not up yet, a transient 401);
  // initConnection retries that until it lands.
  let connectRetry: ReturnType<typeof setTimeout> | undefined;

  async function loadTargets(): Promise<boolean> {
    try {
      const [ps, zs] = await Promise.all([api.players(), api.zones()]);
      players = ps;
      zones = zs;
      const browser = ps.find((p) => p.kind === "browser");
      player.browserId = browser?.id ?? null;
      player.browserZoneId = browser?.zone_id ?? null;
      return true;
    } catch {
      return false; // keep previous lists
    }
  }

  async function initConnection() {
    clearTimeout(connectRetry);
    if (await loadTargets()) {
      // Keep the explicitly chosen output on this device across page reloads.
      try {
        const saved = localStorage.getItem("musicata.output");
        const target = [...players.map(p => ({kind: "player" as const, id: p.id})),
          ...zones.map(z => ({kind: "zone" as const, id: z.id}))]
          .find(t => `${t.kind}:${t.id}` === saved);
        if (target) {
          connect(target.kind, target.id);
          return;
        }
      } catch { /* Storage may be unavailable; use the normal default. */ }
      const browser = players.find((p) => p.kind === "browser") ?? players[0];
      if (browser) {
        connect("player", browser.id); // the WS owns reconnection from here
        return;
      }
    }
    // Couldn't reach the server (or no player yet): show it and retry.
    player.connection = "reconnecting";
    connectRetry = setTimeout(initConnection, 2000);
  }

  let renderer: ReturnType<typeof connectBrowserRenderer> | null = null;

  function connect(kind: "player" | "zone", id: string) {
    renderer?.close();
    renderer = null;
    // Stop the previous local stream before binding its events to another target.
    audio?.pause();
    resumeOnReconnect = false;
    player.playBlocked = false;
    ws?.close();
    player.targetEpoch += 1;
    player.activeKind = kind;
    player.activeId = id;
    player.playback = null;
    player.elapsed = 0;
    player.duration = 0;
    player.connection = "connecting";
    const output = kind === "player" ? players.find(p => p.id === id) : players.find(p => p.zone_id === id);
    zoneMemberId = kind === "zone" ? output?.id ?? null : null;
    if (output) dsp.bindOutput(output.id, output.name);
    else outputAudio.close();
    if (audio && player.isBrowserOutput && player.browserId) renderer = connectBrowserRenderer(player.browserId, audio);
    ws = connectPlayer(kind, id, {
      onState: applyState,
      onProgress: applyTick,
      onDisconnect: () => {
        // Server went away — surface it and don't let buffered audio keep playing on this tab.
        // connectPlayer keeps retrying to the same (stable) id, so this self-heals on its own.
        player.connection = "reconnecting";
        if (player.isBrowserOutput) {
          // Remember we were playing so we resume once the (restarted) server is back.
          if (player.status === "playing") resumeOnReconnect = true;
          audio?.pause();
        }
      },
    });
  }

  function onTargetChange(value: string) {
    const separator = value.indexOf(":");
    const kind = value.slice(0, separator) as "player" | "zone";
    const id = value.slice(separator + 1);
    connect(kind, id);
    try { localStorage.setItem("musicata.output", value); } catch { /* Optional preference. */ }
  }

  onDestroy(() => {
    search.cancelPending();
    clearTimeout(connectRetry);
    ws?.close();
    renderer?.close();
    outputAudio.close();
    audio?.stop();
  });
</script>

<svelte:window onpopstate={async (event) => {
  const wasNowPlaying = nowPlayingOpen;
  const wasNavigation = navOpen;
  nav.restore(event.state);
  await tick();
  if (wasNowPlaying && !nowPlayingOpen) document.querySelector<HTMLButtonElement>('.now-title-button')?.focus({preventScroll: true});
  else if (wasNavigation && !navOpen) document.querySelector<HTMLButtonElement>('[aria-label="Open navigation"]')?.focus({preventScroll: true});
}} onkeydown={(event) => {
  if ($activeModal) return;
  if (event.key === "Escape") {
    if (document.querySelector(".track-menu[open], .output-picker:popover-open")) return;
    if (statsPanel.open) { statsPanel.open = false; return; }
    if (meter.open) { meter.toggle(); return; }
    if (dsp.panelOpen) { dsp.panelOpen = false; return; }
    if (metadata.trackId) { metadata.close(); return; }
    if (player.queueOpen) player.queueOpen = false;
    else if (nowPlayingOpen) void setNowPlaying(false);
    else void closeNavigation();
  }
}} />

<main inert={$activeModal !== null} class="shell listening-shell" class:nav-open={navOpen} class:np-open={nowPlayingOpen}>
  <header class="mobile-bar" inert={navOpen}>
    {#if nav.canGoBack}<button class="bar-icon mobile-back" type="button" aria-label="Back" onclick={() => nav.pop()}>‹</button>{/if}
    <button class="bar-icon" type="button" aria-label="Open navigation" aria-expanded={navOpen} onclick={openNavigation}><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M4 6h16M4 12h16M4 18h16"/></svg></button>
    <span class="mobile-brand">Musicata</span>
    <nav class="desktop-navigation" aria-label="Main navigation">
      <button type="button" data-activity="browse" aria-current={!nowPlayingOpen && activity === "browse" ? "page" : undefined} onclick={() => search.clear()}>Library</button>
      <button type="button" aria-current={nowPlayingOpen ? "page" : undefined} onclick={() => setNowPlaying(true)}>Now Playing</button>
      <button type="button" aria-current={!nowPlayingOpen && ["playlists", "playlist", "smart"].includes(route.name) ? "page" : undefined} onclick={() => nav.root({name: "playlists"})}>Playlists</button>
    </nav>
    {#if session.isAdmin}<button class="bar-icon" type="button" aria-label="Settings" data-activity="settings" onclick={() => nav.push({name: "settings", category: "sources"})}><svg viewBox="0 0 24 24" aria-hidden="true"><path d="m9 3-1 3-3 1 1 3-2 2 2 2-1 3 3 1 1 3h6l1-3 3-1-1-3 2-2-2-2 1-3-3-1-1-3Z"/><circle cx="12" cy="12" r="3"/></svg></button>{/if}
  </header>
  <button class="scrim" type="button" hidden={!navOpen} aria-label="Dismiss navigation" onclick={closeNavigation}></button>
  <Sidebar onclose={closeNavigation} inert={!navOpen} />

  <section class="content" inert={navOpen || (nowPlayingOpen && !auxiliaryOpen && !player.queueOpen) || (mobile && panelOpen)}>
    {#if activity !== "settings"}<InstallPrompt inline />{/if}
    <div class="browse-toolbar">
    <header class="content-header">
      {#if nav.canGoBack}
        <button class="back-btn" type="button" onclick={() => nav.pop()}>‹ Back</button>
      {/if}
      <div class="content-title">
        {#if route.name !== "album" && route.name !== "artist"}<h2>{title}</h2>{/if}
      </div>
      {#if activity === "browse"}
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
      </div>
      {/if}
    </header>

    {#if activity === "browse"}
      <form class="search library-search" role="search" onsubmit={event => { event.preventDefault(); search.update(search.draft); if (mobile) event.currentTarget.querySelector("input")?.blur(); }}>
        <label for="library-search">Search library</label>
        <div class="search-field">
          <svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="10" cy="10" r="6"/><path d="m15 15 5 5"/></svg>
          <input id="library-search" type="search" aria-label="Search library" placeholder="Artists, albums, tracks…" autocomplete="off"
            bind:value={search.draft} oninput={event => search.schedule(event.currentTarget.value)} />
          {#if search.draft || search.query}<button class="ghost-button clear-search" type="button" aria-label="Clear search" onclick={async () => { search.clear(); await tick(); document.getElementById("library-search")?.focus({preventScroll:true}); }}>Clear</button>{/if}
        </div>
      </form>
    {/if}

    </div>

    {#if activity === "listen"}
      <nav class="listen-tabs" aria-label="Listening activities">
        {#each listenTabs as tab}
          <button class="ghost-button" type="button" data-listen-tab={tab.name} aria-pressed={route.name === tab.name}
            onclick={() => nav.root({name: tab.name})}>{tab.label}</button>
        {/each}
      </nav>
    {/if}

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
    {:else if route.name === "playlists"}
      <PlaylistsView />
    {:else if route.name === "playlist"}
      <PlaylistView id={route.id} />
    {:else if route.name === "smart"}
      <SmartPlaylistView id={route.id} />
    {:else if route.name === "radio"}
      <RadioView />
    {:else if route.name === "mix"}
      <MixView />
    {:else if route.name === "queue"}
      <QueueDrawer embedded />
    {:else if route.name === "settings"}
      {#if session.isAdmin}<SettingsWorkspace category={route.category} />
      {:else}<p>Administrator access required.</p>{/if}
    {/if}
  </section>

  <aside class="playback-panels" class:open={panelOpen} class:listening={nowPlayingOpen && !auxiliaryOpen && !player.queueOpen} aria-label="Playback details" inert={navOpen}>
    {#if nowPlayingOpen}<NowPlaying hidden={player.queueOpen || auxiliaryOpen} onclose={() => setNowPlaying(false)} />{/if}
    <MetadataPanel />
    <QueueDrawer />
    <EqPanel />
    <VuMeter />
    <StatsPanel />
  </aside>
  <div class="player-bar" inert={navOpen}>
    <div class="player-destination">
      <OutputPicker {players} {zones} memberId={zoneMemberId} onmemberchange={member => { zoneMemberId = member.id; dsp.bindOutput(member.id, member.name); }} onchange={onTargetChange} onrefresh={loadTargets} />
    </div>
    <Footer onexpand={() => setNowPlaying(true)} onqueue={toggleQueue} />
  </div>
  <nav class="mobile-tabs" aria-label="Main navigation" inert={navOpen}>
    <button type="button" data-tab="library" aria-current={!nowPlayingOpen && !["playlists", "playlist", "smart"].includes(route.name) ? "page" : undefined} class:active={!nowPlayingOpen && !["playlists", "playlist", "smart"].includes(route.name)} onclick={() => search.clear()}>Library</button>
    <button type="button" data-tab="playlists" aria-current={!nowPlayingOpen && ["playlists", "playlist", "smart"].includes(route.name) ? "page" : undefined} class:active={!nowPlayingOpen && ["playlists", "playlist", "smart"].includes(route.name)} onclick={() => nav.root({name: "playlists"})}>Playlists</button>
    <button type="button" data-tab="now-playing" aria-current={nowPlayingOpen ? "page" : undefined} class:active={nowPlayingOpen} onclick={() => setNowPlaying(true)}>Now Playing</button>
  </nav>
</main>

<Modal />
<audio bind:this={audioEl} preload="none" hidden></audio>

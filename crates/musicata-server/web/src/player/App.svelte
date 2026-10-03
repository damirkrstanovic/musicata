<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { onMount, onDestroy, tick, untrack } from "svelte";
  import { api } from "../lib/api";
  import { player } from "../lib/player.svelte";
  import { nav } from "../lib/nav.svelte";
  import { connectPlayer, type PlayerSocket, type ProgressTick } from "../lib/playerWs";
  import { BrowserAudio } from "../lib/audio";
  import { setAudio, resume, pause, next as skipNext, previous as skipPrevious } from "../lib/playback";
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
  import StatsPanel from "./StatsPanel.svelte";
  import MetadataPanel from "./MetadataPanel.svelte";
  import FavoritesView from "./FavoritesView.svelte";
  import PlaylistView from "./PlaylistView.svelte";
  import SmartPlaylistView from "./SmartPlaylistView.svelte";
  import MixView from "./MixView.svelte";
  import InstallPrompt from "./InstallPrompt.svelte";
  import { install } from "../lib/install.svelte";
  import { audioDevices } from "../lib/audioDevices.svelte";

  let audioEl: HTMLAudioElement;
  let audio: BrowserAudio | null = null;
  let ws: PlayerSocket | null = null;
  let navOpen = $state(false);
  let nowPlayingOpen = $state(false);
  let mobile = $state(false);

  onMount(() => {
    const query = matchMedia("(max-width: 820px)");
    const update = () => { mobile = query.matches; };
    update();
    query.addEventListener("change", update);
    return () => query.removeEventListener("change", update);
  });

  async function setNowPlaying(open: boolean) {
    nowPlayingOpen = open;
    await tick();
    document.querySelector<HTMLButtonElement>(`[aria-label="${open ? "Close" : "Open"} Now Playing"]`)?.focus();
  }

  async function openOutput() {
    await setNowPlaying(true);
    document.querySelector<HTMLSelectElement>('.player-switch-btn')?.focus();
  }

  async function openNavigation() {
    navOpen = true;
    await tick();
    document.querySelector<HTMLButtonElement>('[aria-label="Close navigation"]')?.focus();
  }

  async function closeNavigation() {
    if (!navOpen) return;
    navOpen = false;
    await tick();
    document.querySelector<HTMLButtonElement>('[aria-label="Open navigation"]')?.focus();
  }

  // Choosing a saved playlist/view should reveal it behind the mobile drawer.
  $effect(() => {
    nav.current;
    untrack(() => { void closeNavigation(); });
  });
  // Set when the connection drops while this tab was the playing output, so we resume on
  // reconnect: a restarted server restores the session as *paused* (it can't know a tab is still
  // here), which would otherwise leave the footer paused while audio was mid-track.
  let resumeOnReconnect = false;

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
    audio.setEq(dsp.enabled ? dsp.active : null); // apply persisted profile on load
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
    audioDevices.applyActive(false);
  });

  // Players + zones for the output switcher.
  let players = $state<Player[]>([]);
  let zones = $state<Zone[]>([]);

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

  function connect(kind: "player" | "zone", id: string) {
    // Stop the previous local stream before binding its events to another target.
    audio?.pause();
    resumeOnReconnect = false;
    player.playBlocked = false;
    ws?.close();
    player.activeKind = kind;
    player.activeId = id;
    player.playback = null;
    player.elapsed = 0;
    player.duration = 0;
    player.connection = "connecting";
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
    clearTimeout(connectRetry);
    ws?.close();
    audio?.stop();
  });
</script>

<svelte:window onpopstate={() => nav.pop()} onkeydown={(event) => {
  if (event.key === "Escape") {
    if (player.queueOpen) player.queueOpen = false;
    else if (nowPlayingOpen) void setNowPlaying(false);
    else void closeNavigation();
  }
}} />

<main class="shell" class:nav-open={navOpen} class:np-open={nowPlayingOpen}>
  <header class="mobile-bar" inert={mobile && (navOpen || nowPlayingOpen || player.queueOpen)}>
    <button class="bar-icon" type="button" aria-label="Open navigation" aria-expanded={navOpen} onclick={openNavigation}>☰</button>
    <button class="mobile-output" type="button" onclick={openOutput} aria-expanded={nowPlayingOpen}>
      <span>Output:</span>
      <strong>{(player.activeKind === "zone" ? zones : players).find(p => p.id === player.activeId)?.name ?? "Connecting…"}</strong>
      <span aria-hidden="true">⌄</span>
    </button>
    <a class="bar-icon" href="/admin" aria-label="Settings">⚙</a>
  </header>
  <button class="scrim" type="button" hidden={!navOpen} aria-label="Dismiss navigation" onclick={closeNavigation}></button>
  <Sidebar onclose={closeNavigation} inert={mobile && !navOpen} />

  <section class="content" inert={mobile && (navOpen || nowPlayingOpen || player.queueOpen)}>
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
    {:else if route.name === "mix"}
      <MixView />
    {/if}
  </section>

  <aside class="right-rail" class:overlay-open={player.queueOpen} inert={mobile && navOpen}>
    <div class="rail-header" inert={mobile && player.queueOpen}>
      <button class="np-chevron" type="button" aria-label="Close Now Playing" onclick={() => setNowPlaying(false)}>⌄</button>
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
      </div>
    </div>
    <div class="rail-top">
      <MetadataPanel />
      <QueueDrawer />
      <EqPanel />
      <VuMeter />
      <StatsPanel />
    </div>
    <Footer onexpand={() => setNowPlaying(true)} inert={mobile && player.queueOpen} />
  </aside>
</main>

<Modal />
{#if !navOpen && !nowPlayingOpen && !player.queueOpen}<InstallPrompt />{/if}
<audio bind:this={audioEl} preload="none" hidden></audio>

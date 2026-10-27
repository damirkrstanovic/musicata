<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { player } from "../lib/player.svelte";
  import { autoplay } from "../lib/autoplay.svelte";
  import { sendCommand } from "../lib/commands";
  import { playQueueIndex } from "../lib/playback";
  import { api } from "../lib/api";
  import { nav } from "../lib/nav.svelte";
  import { promptText, confirmAction } from "../lib/modal";
  import { initial } from "../lib/dom";
  import { tick, type Snippet } from "svelte";

  let { embedded = false, listening = false, visible = true, controls }: { embedded?: boolean; listening?: boolean; visible?: boolean; controls?: Snippet } = $props();
  let queueList = $state<HTMLDivElement | null>(null);
  let revealFor: string | null = null;
  let wasStandaloneQueueOpen = false;
  let currentVisible = $state(true);
  let editing = $state(false);
  let optionsOpen = $state(false);
  let busy = $state(false);
  let message = $state("");
  let cleared = $state<{ epoch: number } | null>(null);

  async function clearQueue() {
    const target = player.target;
    const epoch = player.targetEpoch;
    if (!target || !player.queue.length || busy) return;
    if (!await confirmAction({ title: "Clear this queue?", message: `Remove all ${player.queue.length} items and stop playback? You can undo this while the queue is empty.`, confirmLabel: "Clear queue" })) return;
    if (player.targetEpoch !== epoch) return;
    busy = true;
    const success = await sendCommand(target, { command: "clear" });
    if (player.targetEpoch === epoch) {
      cleared = success ? { epoch } : null;
      message = success ? "Queue cleared." : "Could not clear the queue. Try again.";
    }
    busy = false;
  }

  async function undoClear() {
    const saved = cleared;
    if (!saved || busy || player.queue.length || player.targetEpoch !== saved.epoch) return;
    busy = true;
    // The server retains the exact cleared items, including continuous-play provenance.
    const success = await sendCommand(player.target, { command: "restore_cleared_queue" });
    if (player.targetEpoch === saved.epoch) {
      message = success ? "Queue restored. Choose a track to play." : "Could not restore the queue. Try again.";
      if (success) cleared = null;
    }
    busy = false;
  }

  function updateCurrentVisibility() {
    const current = queueList?.querySelector<HTMLElement>('.queue-row.current');
    if (!queueList || !current) {
      currentVisible = true;
      return;
    }
    const list = queueList.getBoundingClientRect();
    const row = current.getBoundingClientRect();
    currentVisible = row.top >= list.top && row.bottom <= list.bottom;
  }

  function revealCurrent() {
    const current = queueList?.querySelector<HTMLElement>('.queue-row.current');
    if (!queueList || !current) return;
    const list = queueList.getBoundingClientRect();
    const row = current.getBoundingClientRect();
    queueList.scrollTop += row.top - list.top - (list.height - row.height) / 2;
    updateCurrentVisibility();
  }

  // Reveal once after this visible queue receives its first snapshot. Do not follow later
  // playback ticks or manual scrolling: the listener owns the reading position after that.
  $effect(() => {
    const shown = embedded ? visible : player.queueOpen;
    const snapshot = player.playback;
    const key = `${player.targetEpoch}:${shown}`;
    if (!shown) {
      revealFor = null;
      return;
    }
    if (!snapshot || player.connection !== "online" || revealFor === key) return;
    revealFor = key;
    void tick().then(() => {
      if (revealFor === key) revealCurrent();
    });
  });

  // A current-track change only refreshes the Jump button. It never scrolls the reader back
  // to the new current item after the initial reveal.
  $effect(() => {
    const snapshot = player.playback;
    const position = player.queuePosition;
    const queueLength = player.queue.length;
    if (!snapshot || position < 0 || queueLength === 0) return;
    void tick().then(updateCurrentVisibility);
  });

  $effect(() => {
    const list = queueList;
    if (!list || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(updateCurrentVisibility);
    observer.observe(list);
    return () => observer.disconnect();
  });

  // The automatic reveal must leave the Queue trigger focused. Returning focus after an
  // explicit close remains useful for keyboard listeners.
  $effect(() => {
    if (embedded) return;
    const open = player.queueOpen;
    const returnFocus = wasStandaloneQueueOpen && !open;
    wasStandaloneQueueOpen = open;
    if (!returnFocus) return;
    void tick().then(() => {
      if (player.queueOpen) return;
      [...document.querySelectorAll<HTMLButtonElement>('.mini-controls [aria-label="Queue"], .queue-btn')]
        .find(button => button.getClientRects().length > 0 && !button.closest('[inert]'))?.focus();
    });
  });

  function close() {
    player.queueOpen = false;
  }

  async function saveQueue() {
    const trackIds = player.queue.flatMap((item) => item.track_id ? [item.track_id] : []);
    if (!trackIds.length) return;
    const name = await promptText({ title: "Save queue as playlist", label: "Name", confirmLabel: "Save" });
    if (!name) return;
    const playlist = await api.createPlaylist(name, trackIds);
    if (playlist) {
      nav.push({ name: "playlist", id: playlist.id, label: playlist.name });
    }
  }
</script>

{#snippet actions()}
      <div class="queue-head-actions">
        <button class="ghost-button queue-edit" type="button" aria-pressed={editing} onclick={() => editing = !editing}>{editing ? "Done editing" : "Edit queue"}</button>
        <button class="ghost-button queue-options-toggle" type="button" aria-label="Queue options" aria-expanded={optionsOpen} onclick={() => optionsOpen = !optionsOpen}>•••</button>
        {#if !embedded}<button class="ghost-button" type="button" aria-label="Close queue" onclick={close}>Close</button>{/if}
      </div>
{/snippet}

{#if embedded || player.queueOpen}
  <section class="queue-drawer" class:embedded aria-label="Play queue">
    <header class="queue-head">
      <strong>Queue</strong>
      {#if controls}{@render controls()}{/if}
      {#if listening}
        <button class="ghost-button queue-options-toggle" type="button" aria-label="Queue options" aria-expanded={optionsOpen} onclick={() => optionsOpen = !optionsOpen}>•••</button>
      {:else}{@render actions()}{/if}
    </header>
    <div class="continuous-play-row">
      <button class="continuous-play-control" type="button" aria-pressed={autoplay.enabled ?? undefined}
        disabled={autoplay.enabled === null || autoplay.busy} onclick={() => autoplay.toggle()}>
        {autoplay.error ? "Continuous play unavailable" : autoplay.enabled === null ? "Continuous play loading…" : `Continuous play ${autoplay.enabled ? "on" : "off"}`}
        <small>All outputs</small>
      </button>
      {#if player.queuePosition >= 0 && player.queue.length > 0}
        <button class="ghost-button jump-current" class:current-visible={currentVisible} type="button" aria-label="Jump to current song" onclick={revealCurrent}>Jump to current</button>
      {/if}
    </div>
    {#if autoplay.error}
      <p class="continuous-play-error" role="status">{autoplay.error} <button type="button" onclick={() => autoplay.load()}>Retry</button></p>
    {/if}
    {#if optionsOpen}
      <div class="queue-options">
        {#if listening}<button class="ghost-button queue-edit" type="button" aria-pressed={editing} onclick={() => { editing = !editing; optionsOpen = false; }}>{editing ? "Done editing" : "Edit queue"}</button>{/if}
        <button class="ghost-button save-queue-playlist" type="button" disabled={!player.queue.some(item => item.track_id)} onclick={saveQueue}>Save as playlist</button>
        <button class="ghost-button clear-queue" type="button" disabled={busy || !player.queue.length} onclick={clearQueue}>Clear queue…</button>
        <p class="autoplay-toggle">Turning it off stops future additions. Songs already in this queue stay here.</p>
      </div>
    {/if}
    {#if message}
      <div class="queue-feedback" role="status"><span>{message}</span>
        {#if cleared && cleared.epoch === player.targetEpoch && !player.queue.length}<button class="ghost-button undo-clear" type="button" disabled={busy} onclick={undoClear}>Undo</button>{/if}
      </div>
    {/if}
    {#if player.queueActivity}
      <p class="queue-activity" role="status" aria-live="polite">{player.queueActivity}</p>
    {/if}

    {#if player.queue.length === 0}
      <p class="queue-empty">The queue is empty.</p>
    {:else}
      <div class="queue-list" bind:this={queueList} onscroll={updateCurrentVisibility}>
        {#each player.queue as item, index (index)}
          <div class="queue-row" class:current={index === player.queuePosition} aria-current={index === player.queuePosition ? "true" : undefined} data-index={index}>
            <span class="q-index">{index === player.queuePosition ? "▶" : index + 1}</span>
            <span class="q-art">
              {#if item.artwork_url}<img src={item.artwork_url} alt="" />{:else}{initial(item.title)}{/if}
            </span>
            <button
              class="q-main"
              type="button"
              onclick={() => playQueueIndex(index)}
            >
              <span class="q-title">{item.title || "Unknown"}</span>
              <span class="q-sub">{[item.artist, item.album].filter(Boolean).join(" · ")}</span>
              {#if item.automatically_added}<span class="queue-origin">Added by continuous play</span>{/if}
            </button>
            <span class="q-actions" hidden={!editing}>
              <button
                class="icon-button"
                type="button"
                title="Move up" aria-label="Move up"
                disabled={index === 0}
                onclick={() => sendCommand(player.target, { command: "move_queue_item", from: index, to: index - 1 })}
                >↑</button
              >
              <button
                class="icon-button"
                type="button"
                title="Move down" aria-label="Move down"
                disabled={index === player.queue.length - 1}
                onclick={() => sendCommand(player.target, { command: "move_queue_item", from: index, to: index + 1 })}
                >↓</button
              >
              <button
                class="icon-button"
                type="button"
                title="Remove" aria-label="Remove"
                onclick={() => sendCommand(player.target, { command: "remove_queue_item", index })}>×</button
              >
            </span>
          </div>
        {/each}
      </div>
    {/if}
  </section>
{/if}

<style>
  .queue-head { flex-wrap:nowrap; }
  .queue-head-actions { gap:4px; }
  .queue-options-toggle { min-width:44px; min-height:44px; }
  .queue-head-actions button { min-height:44px; padding:8px; }
  .queue-options { display:flex; flex-wrap:wrap; align-items:center; gap:8px; padding:0 16px 12px; }
  .continuous-play-row { display:flex; flex-wrap:wrap; align-items:center; gap:6px; padding:4px 8px; border-bottom:1px solid var(--line); color:var(--muted); font-size:.8rem; }
  .continuous-play-control { min-height:44px; border:1px solid var(--line); border-radius:6px; background:transparent; color:var(--text); font:inherit; font-size:.75rem; padding:6px 8px; cursor:pointer; }
  .continuous-play-control small { display:block; color:var(--muted); font-size:.68rem; }
  .continuous-play-control[aria-pressed="true"] { border-color:var(--accent); color:var(--accent); }
  .continuous-play-control:disabled { cursor:wait; opacity:.7; }
  .continuous-play-error { margin:0; padding:8px 16px; color:var(--danger); font-size:.82rem; }
  .continuous-play-error button { border:0; background:none; color:inherit; font:inherit; text-decoration:underline; cursor:pointer; }
  .autoplay-toggle { margin:0; color:var(--muted); font-size:.8rem; }
  .queue-origin { color:var(--accent); font-size:.72rem; }
  .jump-current { min-height:44px; margin-left:auto; padding:6px 8px; font:inherit; font-size:.75rem; }
  .jump-current.current-visible { color:var(--muted); }
  .queue-feedback { display:flex; align-items:center; gap:8px; padding:8px 16px; color:var(--muted); font-size:.85rem; }
  .queue-activity { margin: 0.7rem 0; color: var(--muted); }
</style>

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

  let { embedded = false, listening = false, controls }: { embedded?: boolean; listening?: boolean; controls?: Snippet } = $props();
  let wasOpen = false;
  let editing = $state(false);
  let optionsOpen = $state(false);
  let busy = $state(false);
  let message = $state("");
  let cleared = $state<{ ids: string[]; epoch: number } | null>(null);

  async function clearQueue() {
    const target = player.target;
    const epoch = player.targetEpoch;
    if (!target || !player.queue.length || busy) return;
    const canRestore = player.queue.every(item => item.track_id);
    if (!await confirmAction({ title: "Clear this queue?", message: `Remove all ${player.queue.length} items and stop playback?${canRestore ? " You can undo this while the queue is empty." : " Live streams cannot be restored with Undo."}`, confirmLabel: "Clear queue" })) return;
    if (player.targetEpoch !== epoch) return;
    const ids = player.queue.flatMap(item => item.track_id ? [item.track_id] : []);
    busy = true;
    const success = await sendCommand(target, { command: "clear" });
    if (player.targetEpoch === epoch) {
      cleared = success && canRestore ? { ids, epoch } : null;
      message = success ? "Queue cleared." : "Could not clear the queue. Try again.";
    }
    busy = false;
  }

  async function undoClear() {
    const saved = cleared;
    if (!saved || busy || player.queue.length || player.targetEpoch !== saved.epoch) return;
    busy = true;
    // Enqueue restores the saved order without starting audio outside a playback gesture.
    const success = await sendCommand(player.target, { command: "enqueue", track_ids: saved.ids });
    if (player.targetEpoch === saved.epoch) {
      message = success ? "Queue restored. Choose a track to play." : "Could not restore the queue. Try again.";
      if (success) cleared = null;
    }
    busy = false;
  }

  $effect(() => {
    if (embedded) return;
    const open = player.queueOpen;
    if (!open && !wasOpen) return;
    wasOpen = open;
    void tick().then(() => {
      if (player.queueOpen !== open) return;
      if (open) {
        document.querySelector<HTMLButtonElement>('[aria-label="Close queue"]')?.focus();
      } else {
        [...document.querySelectorAll<HTMLButtonElement>('.mini-controls [aria-label="Queue"], .queue-btn')]
          .find(button => button.getClientRects().length > 0 && !button.closest('[inert]'))?.focus();
      }
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
      {#if !listening}{@render actions()}{/if}
    </header>
    {#if optionsOpen}
      <div class="queue-options">
        <button class="ghost-button save-queue-playlist" type="button" disabled={!player.queue.some(item => item.track_id)} onclick={saveQueue}>Save as playlist</button>
        <button class="ghost-button clear-queue" type="button" disabled={busy || !player.queue.length} onclick={clearQueue}>Clear queue…</button>
        <label class="autoplay-toggle" title="Keep playing similar tracks when the queue ends"><input type="checkbox" checked={autoplay.enabled} onchange={() => autoplay.toggle()} /><span>Autoplay</span></label>
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
      <div class="queue-list">
        {#each player.queue as item, index (index)}
          <div class="queue-row" class:current={index === player.queuePosition} data-index={index}>
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
    {#if listening}<footer class="queue-footer">{@render actions()}</footer>{/if}
  </section>
{/if}

<style>
  .queue-head { flex-wrap:nowrap; }
  .queue-head-actions { gap:4px; }
  .queue-head-actions button { min-height:44px; padding:8px; }
  .queue-options { display:flex; flex-wrap:wrap; align-items:center; gap:8px; padding:0 16px 12px; }
  .queue-feedback { display:flex; align-items:center; gap:8px; padding:8px 16px; color:var(--muted); font-size:.85rem; }
  .queue-activity { margin: 0.7rem 0; color: var(--muted); }
</style>

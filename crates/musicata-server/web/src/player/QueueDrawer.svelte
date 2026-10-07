<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { player } from "../lib/player.svelte";
  import { autoplay } from "../lib/autoplay.svelte";
  import { sendCommand } from "../lib/commands";
  import { playQueueIndex } from "../lib/playback";
  import { api } from "../lib/api";
  import { nav } from "../lib/nav.svelte";
  import { promptText } from "../lib/modal";
  import { initial } from "../lib/dom";
  import { tick } from "svelte";

  let { embedded = false }: { embedded?: boolean } = $props();
  let wasOpen = false;
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

{#if embedded || player.queueOpen}
  <section class="queue-drawer" class:embedded aria-label="Play queue">
    <header class="queue-head">
      <strong>Queue</strong>
      <div class="queue-head-actions">
        <label class="autoplay-toggle" title="Keep playing similar tracks when the queue ends">
          <input
            type="checkbox"
            checked={autoplay.enabled}
            onchange={() => autoplay.toggle()}
          />
          <span>Autoplay</span>
        </label>
        <button class="ghost-button" type="button" onclick={() => sendCommand(player.target, { command: "clear" })}>
          Clear
        </button>
        <button class="ghost-button save-queue-playlist" type="button" disabled={!player.queue.some((item) => item.track_id)} onclick={saveQueue}>
          Save as playlist
        </button>
        {#if !embedded}<button class="ghost-button" type="button" aria-label="Close queue" onclick={close}>Close</button>{/if}
      </div>
    </header>
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
            <span class="q-actions">
              <button
                class="icon-button"
                type="button"
                title="Move up"
                disabled={index === 0}
                onclick={() => sendCommand(player.target, { command: "move_queue_item", from: index, to: index - 1 })}
                >↑</button
              >
              <button
                class="icon-button"
                type="button"
                title="Move down"
                disabled={index === player.queue.length - 1}
                onclick={() => sendCommand(player.target, { command: "move_queue_item", from: index, to: index + 1 })}
                >↓</button
              >
              <button
                class="icon-button"
                type="button"
                title="Remove"
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
  .queue-activity { margin: 0.7rem 0; color: var(--muted); }
</style>

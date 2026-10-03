<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { player } from "../lib/player.svelte";
  import { autoplay } from "../lib/autoplay.svelte";
  import { sendCommand } from "../lib/commands";
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
        {#if !embedded}<button class="ghost-button" type="button" aria-label="Close queue" onclick={close}>Close</button>{/if}
      </div>
    </header>

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
              onclick={() => sendCommand(player.target, { command: "play_queue_index", index })}
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

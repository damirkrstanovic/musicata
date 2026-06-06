<script lang="ts">
  import { player } from "../lib/player.svelte";
  import { sendCommand } from "../lib/commands";
  import { initial } from "../lib/dom";

  function close() {
    player.queueOpen = false;
  }
</script>

{#if player.queueOpen}
  <div class="scrim" role="presentation" onclick={close}></div>
  <section class="queue-drawer" aria-label="Play queue">
    <header class="queue-head">
      <strong>Queue</strong>
      <div class="queue-head-actions">
        <button class="ghost-button" type="button" onclick={() => sendCommand(player.activeId, { command: "clear" })}>
          Clear
        </button>
        <button class="ghost-button" type="button" onclick={close}>Close</button>
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
              onclick={() => sendCommand(player.activeId, { command: "play_queue_index", index })}
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
                onclick={() => sendCommand(player.activeId, { command: "move_queue_item", from: index, to: index - 1 })}
                >↑</button
              >
              <button
                class="icon-button"
                type="button"
                title="Move down"
                disabled={index === player.queue.length - 1}
                onclick={() => sendCommand(player.activeId, { command: "move_queue_item", from: index, to: index + 1 })}
                >↓</button
              >
              <button
                class="icon-button"
                type="button"
                title="Remove"
                onclick={() => sendCommand(player.activeId, { command: "remove_queue_item", index })}>×</button
              >
            </span>
          </div>
        {/each}
      </div>
    {/if}
  </section>
{/if}

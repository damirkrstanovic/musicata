<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  // A mix is an output-bound, live queue. Refill updates arrive through player state, so this
  // view never falls back to the initial recommendation snapshot.
  import { radioMix } from "../lib/radioMix.svelte";
  import { player } from "../lib/player.svelte";
  import { playQueueIndex } from "../lib/playback";
  import { initial } from "../lib/dom";
  import { api } from "../lib/api";
  import { nav } from "../lib/nav.svelte";
  import { promptText } from "../lib/modal";

  async function saveMix() {
    const trackIds = player.queue.flatMap((item) => item.track_id ? [item.track_id] : []);
    if (!trackIds.length) return;
    const name = await promptText({ title: "Save mix as playlist", label: "Name", confirmLabel: "Save" });
    if (!name) return;
    const playlist = await api.createPlaylist(name, trackIds);
    if (playlist) {
      nav.push({ name: "playlist", id: playlist.id, label: playlist.name });
    }
  }
</script>

{#if radioMix.active && radioMix.matches(player.target)}
  <section class="detail-hero">
    <div class="hero-info">
      <h2 class="hero-title">Mix</h2>
      <p class="hero-sub">{radioMix.seed ? `Sounds like ${radioMix.seed} · ` : ""}{player.queue.length} tracks</p>
        <button class="ghost-button save-mix-playlist" type="button" disabled={!player.queue.some((item) => item.track_id)} onclick={saveMix}>Save as playlist</button>
      {#if radioMix.loading}<p class="admin-hint" data-mix-status="loading" role="status" aria-live="polite">Finding more tracks…</p>{/if}
      {#if player.queueActivity}<p class="admin-hint" data-mix-status="activity" role="status" aria-live="polite">{player.queueActivity}</p>{/if}
      {#if radioMix.empty}<p class="admin-hint" data-mix-status="empty">No tracks found for this mix.</p>{/if}
      {#if radioMix.error}<p class="admin-hint mix-error" data-mix-status="error">{radioMix.error}</p>{/if}
    </div>
  </section>
  {#if player.queue.length}
    <div class="queue-list mix-queue" aria-label="Mix queue">
      {#each player.queue as item, index (index)}
        <div class="queue-row" class:current={index === player.queuePosition} data-index={index}>
          <span class="q-index">{index === player.queuePosition ? "▶" : index + 1}</span>
          <span class="q-art">{#if item.artwork_url}<img src={item.artwork_url} alt="" />{:else}{initial(item.title)}{/if}</span>
          <button class="q-main" type="button" onclick={() => playQueueIndex(index)}>
            <span class="q-title">{item.title || "Unknown"}</span>
            <span class="q-sub">{[item.artist, item.album].filter(Boolean).join(" · ")}</span>
          </button>
        </div>
      {/each}
    </div>
  {:else if !radioMix.loading && !radioMix.error && !radioMix.empty}
    <p class="queue-empty" data-mix-status="empty">This mix has no queued tracks.</p>
  {/if}
{:else if radioMix.active}
  <p class="admin-hint">This mix belongs to a different output.</p>
{:else}
  <p class="admin-hint">Start a mix from a track — the ≈ button — to see it here.</p>
{/if}

<style>
  .mix-error { color: var(--danger); font-family: var(--font-mono); }
  .mix-queue { margin-top: 1rem; }
</style>

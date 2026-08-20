<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { api, type PlaylistDetail } from "../lib/api";
  import { playTracks } from "../lib/playback";
  import { confirmAction } from "../lib/modal";
  import { nav } from "../lib/nav.svelte";
  import TrackList from "./TrackList.svelte";

  let { id }: { id: string } = $props();
  let detail = $state<PlaylistDetail | null>(null);
  let failed = $state(false);

  $effect(() => {
    const pid = id;
    let alive = true;
    detail = null;
    failed = false;
    api
      .playlistDetail(pid)
      .then((d) => {
        if (alive) detail = d;
      })
      .catch(() => {
        if (alive) failed = true;
      });
    return () => {
      alive = false;
    };
  });

  async function remove() {
    if (!detail) return;
    const ok = await confirmAction({
      title: "Delete playlist",
      message: `Delete “${detail.name}”? This can't be undone.`,
    });
    if (!ok) return;
    await api.deletePlaylist(detail.id);
    nav.pop();
  }
</script>

{#if detail}
  <section class="detail-hero">
    <div class="hero-info">
      <h2 class="hero-title">{detail.name}</h2>
      <p class="hero-sub">{detail.tracks.length} tracks</p>
      <div class="hero-actions">
        <button class="primary-button" type="button" onclick={() => detail && playTracks(detail.tracks, 0)}>
          Play
        </button>
        <button class="ghost-button danger" type="button" onclick={remove}>Delete</button>
      </div>
    </div>
  </section>
  <TrackList tracks={detail.tracks} />
{:else if failed}
  <p class="admin-hint">Couldn't load this playlist. Try again.</p>
{:else}
  <p class="admin-hint">Loading…</p>
{/if}

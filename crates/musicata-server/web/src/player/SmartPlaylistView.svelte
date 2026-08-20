<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { api, type SmartPlaylistDetail } from "../lib/api";
  import { playTracks } from "../lib/playback";
  import TrackList from "./TrackList.svelte";

  let { id }: { id: string } = $props();
  let detail = $state<SmartPlaylistDetail | null>(null);
  let failed = $state(false);

  $effect(() => {
    const pid = id;
    let alive = true;
    detail = null;
    failed = false;
    api
      .smartPlaylistDetail(pid)
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
</script>

{#if detail}
  <section class="detail-hero">
    <div class="hero-info">
      <h2 class="hero-title">{detail.name}</h2>
      <p class="hero-sub">{detail.description} · {detail.tracks.length} tracks</p>
      <div class="hero-actions">
        <button class="primary-button" type="button" onclick={() => detail && playTracks(detail.tracks, 0)}>
          Play
        </button>
      </div>
    </div>
  </section>
  <TrackList tracks={detail.tracks} />
{:else if failed}
  <p class="admin-hint">Couldn't load this playlist. Try again.</p>
{:else}
  <p class="admin-hint">Loading…</p>
{/if}

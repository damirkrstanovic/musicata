<script lang="ts">
  import { api, type AlbumDetail } from "../lib/api";
  import { sizedArtwork, initial } from "../lib/dom";
  import { playTracks } from "../lib/playback";
  import TrackList from "./TrackList.svelte";

  let { id }: { id: string } = $props();
  let detail = $state<AlbumDetail | null>(null);

  // Refetch when navigating between albums.
  $effect(() => {
    const albumId = id;
    let alive = true;
    detail = null;
    api
      .albumDetail(albumId)
      .then((d) => {
        if (alive) detail = d;
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  });
</script>

{#if detail}
  {@const cover = sizedArtwork(detail.album.artwork_url, 400)}
  <section class="detail-hero">
    <div class="hero-cover">
      {#if cover}
        <img src={cover} alt="" />
      {:else}
        <span class="album-placeholder">{initial(detail.album.title)}</span>
      {/if}
    </div>
    <div class="hero-info">
      <h2 class="hero-title">{detail.album.title}</h2>
      <p class="hero-sub">
        {detail.album.artist_name}{detail.album.year ? ` · ${detail.album.year}` : ""} · {detail.tracks.length}
        tracks
      </p>
      <div class="hero-actions">
        <button class="primary-button" type="button" onclick={() => detail && playTracks(detail.tracks, 0)}>
          Play
        </button>
      </div>
    </div>
  </section>
  <TrackList tracks={detail.tracks} />
{:else}
  <p class="admin-hint">Loading…</p>
{/if}

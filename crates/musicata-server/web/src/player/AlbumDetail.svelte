<script lang="ts">
  import { api, type AlbumDetail } from "../lib/api";
  import Cover from "./Cover.svelte";
  import { playTracks } from "../lib/playback";
  import TrackList from "./TrackList.svelte";

  let { id }: { id: string } = $props();
  let detail = $state<AlbumDetail | null>(null);
  let failed = $state(false);

  // Refetch when navigating between albums.
  $effect(() => {
    const albumId = id;
    let alive = true;
    detail = null;
    failed = false;
    api
      .albumDetail(albumId)
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
    <div class="hero-cover">
      <Cover url={detail.album.artwork_url} size={400} label={detail.album.title} />
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
{:else if failed}
  <p class="admin-hint">Couldn't load this album. Try again.</p>
{:else}
  <p class="admin-hint">Loading…</p>
{/if}

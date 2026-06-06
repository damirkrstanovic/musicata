<script lang="ts">
  import { api, type ArtistDetail } from "../lib/api";
  import { sizedArtwork, initial } from "../lib/dom";
  import { nav } from "../lib/nav.svelte";
  import AlbumCard from "./AlbumCard.svelte";
  import TrackList from "./TrackList.svelte";

  let { id }: { id: string } = $props();
  let detail = $state<ArtistDetail | null>(null);

  $effect(() => {
    const artistId = id;
    let alive = true;
    detail = null;
    api
      .artistDetail(artistId)
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
  {@const image = sizedArtwork(detail.artist.artwork_url, 400)}
  <section class="detail-hero">
    <div class="hero-cover">
      {#if image}
        <img src={image} alt="" />
      {:else}
        <span class="album-placeholder">{initial(detail.artist.name)}</span>
      {/if}
    </div>
    <div class="hero-info">
      <h2 class="hero-title">{detail.artist.name}</h2>
      <p class="hero-sub">{detail.artist.album_count} albums · {detail.artist.track_count} tracks</p>
    </div>
  </section>

  {#if detail.albums.length}
    <div class="browse-grid">
      {#each detail.albums as album (album.id)}
        <AlbumCard {album} onopen={() => nav.push({ name: "album", id: album.id, title: album.title })} />
      {/each}
    </div>
  {/if}

  {#if detail.tracks.length}
    <TrackList tracks={detail.tracks} />
  {/if}
{:else}
  <p class="admin-hint">Loading…</p>
{/if}

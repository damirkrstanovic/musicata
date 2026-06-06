<script lang="ts">
  import type { Album } from "../types/Album";
  import { api } from "../lib/api";
  import { sizedArtwork, initial } from "../lib/dom";
  import { playTracks } from "../lib/playback";

  let { album, onopen }: { album: Album; onopen: () => void } = $props();
  const cover = $derived(sizedArtwork(album.artwork_url, 300));

  async function playAlbum(event: MouseEvent) {
    event.stopPropagation();
    const { tracks } = await api.albumDetail(album.id);
    playTracks(tracks, 0);
  }
</script>

<div
  class="card album-card"
  role="button"
  tabindex="0"
  onclick={onopen}
  onkeydown={(e) => {
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      onopen();
    }
  }}
>
  <div class="card-cover">
    {#if cover}
      <img src={cover} alt="" loading="lazy" />
    {:else}
      <span class="album-placeholder">{initial(album.title)}</span>
    {/if}
    <button class="card-play" type="button" title="Play album" aria-label="Play album" onclick={playAlbum}>▶</button>
  </div>
  <div class="card-text">
    <strong>{album.title}</strong>
    <span>{album.artist_name}{album.year ? ` · ${album.year}` : ""}</span>
  </div>
</div>

<script lang="ts">
  import type { Artist } from "../types/Artist";
  import { sizedArtwork, initial } from "../lib/dom";

  let { artist, onopen }: { artist: Artist; onopen: () => void } = $props();
  const image = $derived(sizedArtwork(artist.artwork_url, 300));
</script>

<div
  class="card artist-card"
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
  <div class="artist-avatar" aria-hidden="true">
    {#if image}
      <img src={image} alt="" loading="lazy" />
    {:else}
      {initial(artist.name)}
    {/if}
  </div>
  <div class="card-text">
    <strong>{artist.name}</strong>
    <span>{artist.album_count} albums · {artist.track_count} tracks</span>
  </div>
</div>

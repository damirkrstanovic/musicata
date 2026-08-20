<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import type { Artist } from "../types/Artist";
  import Cover from "./Cover.svelte";

  let { artist, onopen }: { artist: Artist; onopen: () => void } = $props();
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
    <Cover url={artist.artwork_url} size={300} label={artist.name} />
  </div>
  <div class="card-text">
    <strong>{artist.name}</strong>
    <span>{artist.album_count} albums · {artist.track_count} tracks</span>
  </div>
</div>

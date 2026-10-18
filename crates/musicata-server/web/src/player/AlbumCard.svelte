<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import type { Album } from "../types/Album";
  import { playAlbum as playAlbumOnOutput } from "../lib/playback";
  import Cover from "./Cover.svelte";

  let { album, onopen, displayTitle = album.title, displayArtistName = album.artist_name, firstStreamUrl }: {
    album: Album;
    onopen: () => void;
    displayTitle?: string;
    displayArtistName?: string;
    firstStreamUrl?: string;
  } = $props();

  function playAlbum(event: MouseEvent) {
    event.stopPropagation();
    void playAlbumOnOutput(album.id, firstStreamUrl);
  }
</script>

<div
  class="card album-card"
  role="button"
  tabindex="0"
  onclick={onopen}
  onkeydown={(e) => {
    if (e.target !== e.currentTarget) return;
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      onopen();
    }
  }}
>
  <div class="card-cover">
    <Cover url={album.artwork_url} size={300} label={displayTitle} />
    <button class="card-play" type="button" title="Play album" aria-label="Play album" disabled={!firstStreamUrl} onclick={playAlbum}>▶</button>
  </div>
  <div class="card-text">
    <strong title={album.title}>{displayTitle}</strong>
    <span title={album.artist_name}>{displayArtistName}{album.year ? ` · ${album.year}` : ""}</span>
    {#if displayTitle === "Unidentified album"}<span>{album.track_count} tracks · Open for details</span>{/if}
  </div>
</div>

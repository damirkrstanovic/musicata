<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import type { Album } from "../types/Album";
  import { api } from "../lib/api";
  import { playTracks } from "../lib/playback";
  import Cover from "./Cover.svelte";

  let { album, onopen }: { album: Album; onopen: () => void } = $props();

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
    if (e.target !== e.currentTarget) return;
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      onopen();
    }
  }}
>
  <div class="card-cover">
    <Cover url={album.artwork_url} size={300} label={album.title} />
    <button class="card-play" type="button" title="Play album" aria-label="Play album" onclick={playAlbum}>▶</button>
  </div>
  <div class="card-text">
    <strong title={album.title}>{album.title}</strong>
    <span title={album.artist_name}>{album.artist_name}{album.year ? ` · ${album.year}` : ""}</span>
    {#if album.title.toLowerCase().startsWith("unknown album")}<span>{album.track_count} tracks · Open for details</span>{/if}
  </div>
</div>

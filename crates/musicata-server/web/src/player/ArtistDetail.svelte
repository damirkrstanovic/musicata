<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { api, type ArtistDetail } from "../lib/api";
  import { nav } from "../lib/nav.svelte";
  import AlbumCard from "./AlbumCard.svelte";
  import Cover from "./Cover.svelte";
  import TrackList from "./TrackList.svelte";

  let { id }: { id: string } = $props();
  let detail = $state<ArtistDetail | null>(null);
  let failed = $state(false);

  $effect(() => {
    const artistId = id;
    let alive = true;
    detail = null;
    failed = false;
    api
      .artistDetail(artistId)
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
    <div class="hero-cover artist">
      <span class="artist-avatar large" aria-hidden="true">
        <Cover url={detail.artist.artwork_url} size={400} label={detail.artist.name} />
      </span>
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
{:else if failed}
  <p class="admin-hint">Couldn't load this artist. Try again.</p>
{:else}
  <p class="admin-hint">Loading…</p>
{/if}

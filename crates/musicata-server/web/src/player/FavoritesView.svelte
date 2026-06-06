<script lang="ts">
  import { api, type Favorites } from "../lib/api";
  import { nav } from "../lib/nav.svelte";
  import AlbumCard from "./AlbumCard.svelte";
  import ArtistCard from "./ArtistCard.svelte";
  import TrackList from "./TrackList.svelte";

  let favs = $state<Favorites | null>(null);
  api
    .favorites()
    .then((f) => (favs = f))
    .catch(() => {});

  const empty = $derived(
    favs !== null && !favs.artists.length && !favs.albums.length && !favs.tracks.length,
  );
</script>

{#if favs}
  {#if favs.artists.length}
    <h3 class="section-title">Artists</h3>
    <div class="browse-grid">
      {#each favs.artists as artist (artist.id)}
        <ArtistCard {artist} onopen={() => nav.push({ name: "artist", id: artist.id, label: artist.name })} />
      {/each}
    </div>
  {/if}
  {#if favs.albums.length}
    <h3 class="section-title">Albums</h3>
    <div class="browse-grid">
      {#each favs.albums as album (album.id)}
        <AlbumCard {album} onopen={() => nav.push({ name: "album", id: album.id, title: album.title })} />
      {/each}
    </div>
  {/if}
  {#if favs.tracks.length}
    <h3 class="section-title">Tracks</h3>
    <TrackList tracks={favs.tracks} />
  {/if}
  {#if empty}<p class="admin-hint">No favorites yet — tap a heart on any track.</p>{/if}
{:else}
  <p class="admin-hint">Loading…</p>
{/if}

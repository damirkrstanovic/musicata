<script lang="ts">
  import { api, type SearchResults } from "../lib/api";
  import { search } from "../lib/search.svelte";
  import { nav } from "../lib/nav.svelte";
  import AlbumCard from "./AlbumCard.svelte";
  import ArtistCard from "./ArtistCard.svelte";
  import TrackList from "./TrackList.svelte";

  let results = $state<SearchResults | null>(null);

  // Refetch as the query changes; an AbortController cancels the in-flight request so a
  // slower earlier response can't overwrite a newer one.
  $effect(() => {
    const q = search.query.trim();
    if (!q) {
      results = null;
      return;
    }
    const controller = new AbortController();
    api
      .search(q, controller.signal)
      .then((r) => (results = r))
      .catch(() => {});
    return () => controller.abort();
  });

  const empty = $derived(
    results !== null &&
      !results.artists.length &&
      !results.albums.length &&
      !results.tracks.length,
  );
</script>

{#if results}
  {#if results.artists.length}
    <h3 class="section-title">Artists</h3>
    <div class="browse-grid">
      {#each results.artists as artist (artist.id)}
        <ArtistCard {artist} onopen={() => nav.push({ name: "artist", id: artist.id, label: artist.name })} />
      {/each}
    </div>
  {/if}
  {#if results.albums.length}
    <h3 class="section-title">Albums</h3>
    <div class="browse-grid">
      {#each results.albums as album (album.id)}
        <AlbumCard {album} onopen={() => nav.push({ name: "album", id: album.id, title: album.title })} />
      {/each}
    </div>
  {/if}
  {#if results.tracks.length}
    <h3 class="section-title">Tracks</h3>
    <TrackList tracks={results.tracks} />
  {/if}
  {#if empty}<p class="admin-hint">No matches.</p>{/if}
{/if}

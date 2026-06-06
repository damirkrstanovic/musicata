<script lang="ts">
  import { api } from "../lib/api";
  import type { Artist } from "../types/Artist";
  import { onVisible } from "../lib/dom";
  import { nav } from "../lib/nav.svelte";
  import ArtistCard from "./ArtistCard.svelte";

  const PAGE = 60;
  let artists = $state<Artist[]>([]);
  let offset = 0;
  let done = $state(false);
  let loading = false;

  async function loadMore() {
    if (done || loading) return;
    loading = true;
    try {
      const page = await api.artists({ limit: PAGE, offset });
      artists = [...artists, ...page.items];
      offset += page.items.length;
      if (page.items.length < PAGE) done = true;
    } catch {
      done = true;
    } finally {
      loading = false;
    }
  }
  loadMore();
</script>

<div class="browse-grid">
  {#each artists as artist (artist.id)}
    <ArtistCard {artist} onopen={() => nav.push({ name: "artist", id: artist.id, label: artist.name })} />
  {/each}
</div>
{#if !done}<div class="scroll-sentinel" use:onVisible={loadMore}></div>{/if}

<script lang="ts">
  import { untrack } from "svelte";
  import { api } from "../lib/api";
  import type { Artist } from "../types/Artist";
  import { onVisible } from "../lib/dom";
  import { nav } from "../lib/nav.svelte";
  import { search } from "../lib/search.svelte";
  import ArtistCard from "./ArtistCard.svelte";

  const PAGE = 60;
  let artists = $state<Artist[]>([]);
  let offset = 0;
  let done = $state(false);
  let loading = false;
  let token = 0;

  async function loadMore() {
    if (done || loading) return;
    loading = true;
    const mine = token;
    try {
      const q = search.query.trim();
      if (q) {
        const results = await api.search(q);
        if (mine !== token) return;
        artists = results.artists;
        done = true;
      } else {
        const page = await api.artists({ limit: PAGE, offset });
        if (mine !== token) return;
        artists = [...artists, ...page.items];
        offset += page.items.length;
        if (page.items.length < PAGE) done = true;
      }
    } catch {
      done = true;
    } finally {
      loading = false;
    }
  }

  $effect(() => {
    void search.query;
    untrack(() => {
      token++;
      artists = [];
      offset = 0;
      done = false;
      loading = false;
      loadMore();
    });
  });
</script>

<div class="browse-grid">
  {#each artists as artist (artist.id)}
    <ArtistCard {artist} onopen={() => nav.push({ name: "artist", id: artist.id, label: artist.name })} />
  {/each}
</div>
{#if !done}<div class="scroll-sentinel" use:onVisible={loadMore}></div>{/if}

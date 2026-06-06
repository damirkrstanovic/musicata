<script lang="ts">
  import { api } from "../lib/api";
  import type { Album } from "../types/Album";
  import { onVisible } from "../lib/dom";
  import { nav } from "../lib/nav.svelte";
  import AlbumCard from "./AlbumCard.svelte";

  const PAGE = 60;
  let albums = $state<Album[]>([]);
  let offset = 0;
  let done = $state(false);
  let loading = false;

  async function loadMore() {
    if (done || loading) return;
    loading = true;
    try {
      const page = await api.albums({ limit: PAGE, offset });
      albums = [...albums, ...page.items];
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
  {#each albums as album (album.id)}
    <AlbumCard {album} onopen={() => nav.push({ name: "album", id: album.id, title: album.title })} />
  {/each}
</div>
{#if !done}<div class="scroll-sentinel" use:onVisible={loadMore}></div>{/if}

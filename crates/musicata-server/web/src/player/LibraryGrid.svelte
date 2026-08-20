<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { untrack } from "svelte";
  import { api } from "../lib/api";
  import type { Album } from "../types/Album";
  import { onVisible } from "../lib/dom";
  import { nav } from "../lib/nav.svelte";
  import { browse } from "../lib/browse.svelte";
  import { search } from "../lib/search.svelte";
  import AlbumCard from "./AlbumCard.svelte";

  const PAGE = 60;
  let albums = $state<Album[]>([]);
  let done = $state(false);
  let offset = 0;
  let loading = false;
  let token = 0; // bumped on filter/search change so stale responses are dropped

  async function loadMore() {
    if (done || loading) return;
    loading = true;
    const mine = token;
    try {
      const q = search.query.trim();
      if (q) {
        const results = await api.search(q);
        if (mine !== token) return;
        albums = results.albums;
        done = true;
      } else {
        const page = await api.albums({
          limit: PAGE,
          offset,
          genre: browse.genre || undefined,
          year: browse.year ?? undefined,
          composer: browse.composer || undefined,
          tag: browse.tag || undefined,
        });
        if (mine !== token) return; // filter changed mid-flight
        albums = [...albums, ...page.items];
        offset += page.items.length;
        if (page.items.length < PAGE) done = true;
      }
    } catch {
      done = true;
    } finally {
      loading = false;
    }
  }

  // Reset and reload whenever the search query or filter changes (also the initial load).
  // The reset + loadMore is untracked so its reads of `done`/`loading` don't become effect
  // deps (which would re-trigger the effect when loadMore flips `done`).
  $effect(() => {
    void search.query;
    void browse.genre;
    void browse.year;
    void browse.composer;
    void browse.tag;
    untrack(() => {
      token++;
      albums = [];
      offset = 0;
      done = false;
      loading = false;
      loadMore();
    });
  });
</script>

<div class="browse-grid">
  {#each albums as album (album.id)}
    <AlbumCard {album} onopen={() => nav.push({ name: "album", id: album.id, title: album.title })} />
  {/each}
</div>
{#if !done}<div class="scroll-sentinel" use:onVisible={loadMore}></div>{/if}

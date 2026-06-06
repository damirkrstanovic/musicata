<script lang="ts">
  import { untrack } from "svelte";
  import { api, type TrackRow } from "../lib/api";
  import { onVisible } from "../lib/dom";
  import { browse } from "../lib/browse.svelte";
  import TrackList from "./TrackList.svelte";

  const PAGE = 100;
  let tracks = $state<TrackRow[]>([]);
  let done = $state(false);
  let offset = 0;
  let loading = false;
  let token = 0;

  async function loadMore() {
    if (done || loading) return;
    loading = true;
    const mine = token;
    try {
      const page = await api.tracks({
        limit: PAGE,
        offset,
        genre: browse.genre || undefined,
        year: browse.year ?? undefined,
        composer: browse.composer || undefined,
      });
      if (mine !== token) return;
      tracks = [...tracks, ...page.items];
      offset += page.items.length;
      if (page.items.length < PAGE) done = true;
    } catch {
      done = true;
    } finally {
      loading = false;
    }
  }

  // Reset + reload on filter change (untracked so loadMore's `done` read isn't a dep).
  $effect(() => {
    void browse.genre;
    void browse.year;
    void browse.composer;
    untrack(() => {
      token++;
      tracks = [];
      offset = 0;
      done = false;
      loading = false;
      loadMore();
    });
  });
</script>

<TrackList {tracks} />
{#if !done}<div class="scroll-sentinel" use:onVisible={loadMore}></div>{/if}

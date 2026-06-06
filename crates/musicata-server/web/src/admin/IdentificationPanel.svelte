<script lang="ts">
  import { onDestroy } from "svelte";
  import {
    api,
    type IdentificationStats,
    type UnidentifiedAlbum,
    type UnidentifiedArtist,
  } from "../lib/api";
  import { pct } from "../lib/format";

  let stats = $state<IdentificationStats | null>(null);
  let albums = $state<UnidentifiedAlbum[]>([]);
  let artists = $state<UnidentifiedArtist[]>([]);

  async function load() {
    try {
      [stats, albums, artists] = await Promise.all([
        api.identificationStats(),
        api.unidentifiedAlbums(25),
        api.unidentifiedArtists(25),
      ]);
    } catch {
      // keep previous
    }
  }
  load();
  const timer = setInterval(load, 30000); // background work progresses
  onDestroy(() => clearInterval(timer));
</script>

<section class="admin-panel">
  <div class="admin-panel-head"><h2>Identification</h2></div>
  <p class="admin-hint">MusicBrainz coverage from audio fingerprinting + text search. Runs in the background after each scan.</p>

  {#if stats}
    <div class="ident-stats">
      <div class="ident-stat">
        <span class="ident-num">{stats.tracks.identified} / {stats.tracks.total}</span>
        <span class="ident-label">Tracks identified ({pct(stats.tracks.identified, stats.tracks.total)}%)</span>
      </div>
      <div class="ident-stat">
        <span class="ident-num">{stats.albums.identified} / {stats.albums.total}</span>
        <span class="ident-label">Albums identified ({pct(stats.albums.identified, stats.albums.total)}%)</span>
      </div>
      <div class="ident-stat">
        <span class="ident-num">{stats.artists.identified} / {stats.artists.total}</span>
        <span class="ident-label">Artists identified ({pct(stats.artists.identified, stats.artists.total)}%)</span>
      </div>
      <div class="ident-stat ident-progress">
        <span class="ident-num">{stats.processed} / {stats.tracks.total}</span>
        <span class="ident-label">Scanned for IDs — {stats.queued} queued</span>
      </div>
    </div>

    <div class="field-grid">
      <div>
        <h3>Most unidentified albums</h3>
        <div class="admin-list">
          {#each albums as album (album.title + album.artist_name)}
            <div class="admin-row">
              <div class="admin-row-main">
                <div class="admin-row-title"><strong>{album.title}</strong></div>
                <span class="ident-label">{album.artist_name} · {album.track_count} tracks</span>
              </div>
            </div>
          {/each}
        </div>
      </div>
      <div>
        <h3>Most unidentified artists</h3>
        <div class="admin-list">
          {#each artists as artist (artist.name)}
            <div class="admin-row">
              <div class="admin-row-main">
                <div class="admin-row-title"><strong>{artist.name}</strong></div>
                <span class="ident-label">{artist.track_count} tracks</span>
              </div>
            </div>
          {/each}
        </div>
      </div>
    </div>
  {:else}
    <p class="admin-hint">Loading…</p>
  {/if}
</section>

<script lang="ts">
  import type { TrackRow } from "../lib/api";
  import { formatTime } from "../lib/format";
  import { playTracks } from "../lib/playback";
  import { player } from "../lib/player.svelte";
  import { nav } from "../lib/nav.svelte";

  // Clicking a row plays the whole list starting there (matches the old player).
  let { tracks }: { tracks: TrackRow[] } = $props();
</script>

<div class="track-list">
  {#each tracks as track, index (track.id)}
    <div class="track" class:active={player.nowPlaying?.track_id === track.id}>
      <div
        class="track-main"
        role="button"
        tabindex="0"
        onclick={() => playTracks(tracks, index)}
        onkeydown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            playTracks(tracks, index);
          }
        }}
      >
        <span class="track-titles">
          <strong>{track.title}</strong>
          <button
            type="button"
            class="track-link"
            onclick={(e) => {
              e.stopPropagation();
              nav.push({ name: "artist", id: track.artist_id, label: track.artist_name });
            }}>{track.artist_name}</button
          >
        </span>
      </div>
      <span class="track-stat">{track.duration_seconds ? formatTime(track.duration_seconds) : ""}</span>
    </div>
  {/each}
</div>

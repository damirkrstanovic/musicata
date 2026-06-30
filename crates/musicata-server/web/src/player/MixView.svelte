<script lang="ts">
  // The current "mix": a seed-based radio (sounds-like / similar). Shows what was generated so it
  // isn't invisible in the queue, and lets you replay it. Populated by startRadio/startAudioRadio.
  import { radioMix } from "../lib/radioMix.svelte";
  import { playTracks } from "../lib/playback";
  import TrackList from "./TrackList.svelte";
</script>

{#if radioMix.active}
  <section class="detail-hero">
    <div class="hero-info">
      <h2 class="hero-title">Mix</h2>
      <p class="hero-sub">Sounds like {radioMix.seed} · {radioMix.tracks.length} tracks</p>
      <div class="hero-actions">
        <button class="primary-button" type="button" onclick={() => playTracks(radioMix.tracks, 0)}>
          Play
        </button>
      </div>
    </div>
  </section>
  <TrackList tracks={radioMix.tracks} />
{:else}
  <p class="admin-hint">Start a mix from a track — the ≈ button — to see it here.</p>
{/if}

<script lang="ts">
  import { api } from "../lib/api";
  import type { RadioStation } from "../types/RadioStation";
  import { playStream } from "../lib/playback";

  let stations = $state<RadioStation[]>([]);
  api
    .radio()
    .then((s) => (stations = s))
    .catch(() => {});
</script>

<h2>Internet radio</h2>
<div class="admin-list">
  {#each stations as station (station.id)}
    <div class="admin-row">
      <button
        class="admin-row-main"
        type="button"
        onclick={() => playStream(station.stream_url, station.name)}
      >
        <div class="admin-row-title"><strong>{station.name}</strong></div>
      </button>
    </div>
  {:else}
    <p class="admin-hint">No stations saved yet.</p>
  {/each}
</div>

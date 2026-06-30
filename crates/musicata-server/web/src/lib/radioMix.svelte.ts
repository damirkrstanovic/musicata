// The current "mix" — a seed-based radio (the seed track plus similar/sounds-like tracks). Held
// here so the Mix view can show what was generated, instead of the tracks vanishing into the
// queue. Transient (the latest mix); replaced each time you start a new one.
import type { TrackRow } from "./api";

class RadioMix {
  /** The seed track's title, e.g. shown as "Sounds like {seed}". */
  seed = $state("");
  tracks = $state<TrackRow[]>([]);

  /** Replace the current mix. `tracks` is seed-first, as returned by the radio endpoints. */
  set(tracks: TrackRow[]): void {
    this.tracks = tracks;
    this.seed = tracks[0]?.title ?? "";
  }

  get active(): boolean {
    return this.tracks.length > 0;
  }
}

export const radioMix = new RadioMix();

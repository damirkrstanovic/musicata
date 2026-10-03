// SPDX-License-Identifier: AGPL-3.0-or-later
// The current seed-based radio request. The output is part of its identity: a late response must
// never start playback on whichever output the user selected while it was loading.
import type { Target } from "./player.svelte";

function sameTarget(a: Target | null, b: Target | null): boolean {
  return a?.kind === b?.kind && a?.id === b?.id;
}

class RadioMix {
  /** The seed track's title, e.g. shown as "Sounds like {seed}". */
  seed = $state("");
  target = $state<Target | null>(null);
  loading = $state(false);
  empty = $state(false);
  error = $state<string | null>(null);
  private request = 0;
  private targetEpoch = 0;

  /** Begin a new request and invalidate every earlier response. */
  begin(target: Target, targetEpoch: number, seed = ""): number {
    this.request += 1;
    this.target = target;
    this.targetEpoch = targetEpoch;
    this.seed = seed;
    this.loading = true;
    this.empty = false;
    this.error = null;
    return this.request;
  }

  isCurrent(request: number, target: Target | null, targetEpoch: number): boolean {
    return this.request === request && this.targetEpoch === targetEpoch && sameTarget(this.target, target);
  }

  finish(request: number, target: Target, targetEpoch: number, seed: string, error: string | null): boolean {
    if (!this.isCurrent(request, target, targetEpoch)) return false;
    this.loading = false;
    this.seed = seed;
    this.error = error;
    return true;
  }

  finishEmpty(request: number, target: Target, targetEpoch: number, seed: string): boolean {
    if (!this.finish(request, target, targetEpoch, seed, null)) return false;
    this.empty = true;
    return true;
  }

  matches(target: Target | null): boolean {
    return sameTarget(this.target, target);
  }

  invalidate(): void {
    this.request += 1;
    this.loading = false;
  }

  cancel(): void {
    this.invalidate();
    this.target = null;
  }

  get active(): boolean {
    return this.target !== null;
  }
}

export const radioMix = new RadioMix();

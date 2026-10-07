// SPDX-License-Identifier: AGPL-3.0-or-later
// Typed mirror of the server's `PlayerCommand` enum (musicata-core), sent to
// POST /api/players/{id}/commands. Hand-typed: it's a tagged enum the client only ever
// *sends*, so a struct-derived type would add no checking the literals don't already give.
import type { RepeatMode } from "../types/RepeatMode";
import type { Target } from "./player.svelte";
import { radioMix } from "./radioMix.svelte";

export type PlayerCommand =
  | { command: "play" }
  | { command: "pause" }
  | { command: "stop" }
  | { command: "next" }
  | { command: "previous" }
  | { command: "seek"; position_seconds: number }
  | { command: "set_volume"; volume: number }
  | { command: "set_repeat"; mode: RepeatMode }
  | { command: "set_shuffle"; enabled: boolean }
  | { command: "clear" }
  | { command: "play_tracks"; track_ids: string[]; start_index: number }
  | { command: "enqueue"; track_ids: string[]; next?: boolean }
  | { command: "play_queue_index"; index: number }
  | { command: "remove_queue_item"; index: number }
  | { command: "move_queue_item"; from: number; to: number }
  | { command: "play_stream"; url: string; title: string };

export async function sendCommand(target: Target | null, command: PlayerCommand): Promise<boolean> {
  if (!target) return false;
  if (command.command === "pause" || command.command === "stop") radioMix.invalidate();
  if (command.command === "clear") radioMix.cancel();
  try {
    const response = await fetch(
      `/api/${target.kind}s/${encodeURIComponent(target.id)}/commands`,
      { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(command) },
    );
    if (!response.ok) {
      const body = await response.json().catch(() => null) as { error?: { message?: string } } | null;
      throw new Error(body?.error?.message ?? `command → ${response.status}`);
    }
    return true;
  } catch (error) {
    console.error("player command failed", command, error);
    return false;
  }
}

// Typed mirror of the server's `PlayerCommand` enum (musicata-core), sent to
// POST /api/players/{id}/commands. Hand-typed: it's a tagged enum the client only ever
// *sends*, so a struct-derived type would add no checking the literals don't already give.
import { api } from "./api";
import type { RepeatMode } from "../types/RepeatMode";

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
  | { command: "enqueue"; track_ids: string[] }
  | { command: "play_queue_index"; index: number }
  | { command: "remove_queue_item"; index: number }
  | { command: "move_queue_item"; from: number; to: number };

export async function sendCommand(playerId: string | null, command: PlayerCommand): Promise<void> {
  if (!playerId) return;
  try {
    await api.playerCommand(playerId, command);
  } catch (error) {
    console.error("player command failed", command, error);
  }
}

// Typed client for the server's JSON API. Response types are generated from the Rust wire
// structs by ts-rs (../types/, via scripts/gen-web-types.sh), so a server-side shape change
// surfaces here as a TypeScript error. The few genuine `json!` endpoints are hand-typed.
import type { LibrarySummary } from "../types/LibrarySummary";
import type { Artist } from "../types/Artist";
import type { Album } from "../types/Album";
import type { Zone } from "../types/Zone";
import type { RadioStation } from "../types/RadioStation";
import type { Player } from "../types/Player";
import type { SourceView } from "../types/SourceView";
import type { AppSettings } from "../types/AppSettings";
import type { ArtistAliasGroup } from "../types/ArtistAliasGroup";
import type { Activity } from "../types/Activity";

/** Server pagination envelope (`Page<T>` in main.rs). Hand-typed — ts-rs doesn't export the
 *  generic wrapper; the element type is generated. */
export interface Page<T> {
  items: T[];
  total: number;
  limit: number;
  offset: number;
  sort: string | null;
}

// A `type` (not `interface`) so it carries an implicit index signature and is accepted
// where a `Record<string, ...>` of query params is expected.
export type ListParams = {
  limit?: number;
  offset?: number;
  sort?: string;
};

// --- Hand-typed `json!` responses (no Rust struct to derive from). ---

export interface IdentificationStats {
  tracks: { total: number; identified: number };
  albums: { total: number; identified: number };
  artists: { total: number; identified: number };
  processed: number;
  queued: number;
  fingerprint: { resolved: number; not_found: number; searched: number };
}

export interface UnidentifiedAlbum {
  title: string;
  artist_name: string;
  track_count: number;
}

export interface UnidentifiedArtist {
  name: string;
  track_count: number;
}

/** The display + playback fields of the server's `Track` — a structural view, so the deep
 *  metadata-observation graph isn't generated until the metadata panel (which consumes it). */
export interface TrackRow {
  id: string;
  title: string;
  artist_id: string;
  artist_name: string;
  album_id: string;
  album_title: string;
  year: number | null;
  track_number: number | null;
  disc_number: number | null;
  duration_seconds: number | null;
  stream_url: string;
}

export interface AlbumDetail {
  album: Album;
  artist: Artist | null;
  tracks: TrackRow[];
}

export interface ArtistDetail {
  artist: Artist;
  albums: Album[];
  tracks: TrackRow[];
}

export interface SearchResults {
  query: string;
  artists: Artist[];
  albums: Album[];
  tracks: TrackRow[];
}

export type { SourceView, AppSettings, ArtistAliasGroup, Activity, Player, Zone };

export interface CreateSourceRequest {
  kind: "smb";
  host?: string;
  share?: string;
  base_path?: string;
  display_name?: string;
  username?: string;
  password?: string;
  domain?: string;
}

export interface MergeArtistsRequest {
  canonical_id: string;
  member_ids: string[];
}

export class ApiError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message);
    this.name = "ApiError";
  }
}

async function getJson<T>(
  path: string,
  params?: Record<string, string | number | undefined>,
  signal?: AbortSignal,
): Promise<T> {
  const url = new URL(path, location.origin);
  for (const [key, value] of Object.entries(params ?? {})) {
    if (value !== undefined) url.searchParams.set(key, String(value));
  }
  const response = await fetch(url, { headers: { accept: "application/json" }, signal });
  if (!response.ok) throw new ApiError(response.status, `GET ${path} → ${response.status}`);
  return (await response.json()) as T;
}

/** POST/PATCH/DELETE with an optional JSON body, surfacing the server's error envelope
 *  (`{error:{message}}`) as the thrown message. Returns null for 204. */
async function sendJson<T>(path: string, method: string, body?: unknown): Promise<T | null> {
  const init: RequestInit = { method };
  if (body !== undefined) {
    init.headers = { "content-type": "application/json" };
    init.body = JSON.stringify(body);
  }
  const response = await fetch(path, init);
  if (!response.ok) {
    let detail = `${response.status} ${response.statusText}`;
    try {
      const payload = (await response.json()) as { error?: { message?: string } };
      if (payload?.error?.message) detail = payload.error.message;
    } catch {
      // keep the status line
    }
    throw new ApiError(response.status, detail);
  }
  if (response.status === 204) return null;
  return (await response.json().catch(() => null)) as T | null;
}

export const api = {
  // Library
  librarySummary: () => getJson<LibrarySummary>("/api/library/summary"),
  artists: (params?: ListParams) => getJson<Page<Artist>>("/api/artists", params),
  albums: (params?: ListParams) => getJson<Page<Album>>("/api/albums", params),
  radio: () => getJson<RadioStation[]>("/api/radio"),

  // Sources
  sources: () => getJson<SourceView[]>("/api/sources"),
  createSource: (body: CreateSourceRequest) => sendJson("/api/sources", "POST", body),
  deleteSource: (id: string) => sendJson(`/api/sources/${encodeURIComponent(id)}`, "DELETE"),
  rescanAll: () => sendJson("/api/sources/all/rescan", "POST"),

  // Settings
  settings: () => getJson<AppSettings>("/api/settings"),
  saveSettings: (body: AppSettings) => sendJson("/api/settings", "PATCH", body),

  albumDetail: (id: string) => getJson<AlbumDetail>(`/api/albums/${encodeURIComponent(id)}`),
  artistDetail: (id: string) => getJson<ArtistDetail>(`/api/artists/${encodeURIComponent(id)}`),
  search: (q: string, signal?: AbortSignal) => getJson<SearchResults>("/api/search", { q }, signal),

  // Players & zones
  players: () => getJson<Player[]>("/api/players"),
  playerCommand: (id: string, command: unknown) =>
    sendJson(`/api/players/${encodeURIComponent(id)}/commands`, "POST", command),
  createPlayer: (body: { kind: "mpd"; address: string; name?: string }) =>
    sendJson("/api/players", "POST", body),
  updatePlayer: (id: string, body: { name?: string; zone_id?: string | null }) =>
    sendJson(`/api/players/${encodeURIComponent(id)}`, "PATCH", body),
  deletePlayer: (id: string) => sendJson(`/api/players/${encodeURIComponent(id)}`, "DELETE"),
  zones: () => getJson<Zone[]>("/api/zones"),
  createZone: (name: string) => sendJson<Zone>("/api/zones", "POST", { name }),
  deleteZone: (id: string) => sendJson(`/api/zones/${encodeURIComponent(id)}`, "DELETE"),

  // Merged artists
  aliases: () => getJson<ArtistAliasGroup[]>("/api/artists/aliases"),
  mergeArtists: (body: MergeArtistsRequest) => sendJson("/api/artists/merge", "POST", body),
  unmergeArtist: (key: string) =>
    sendJson(`/api/artists/aliases/${encodeURIComponent(key)}`, "DELETE"),

  // Activity & identification
  activity: () => getJson<Activity[]>("/api/activity"),
  identificationStats: () => getJson<IdentificationStats>("/api/identification/stats"),
  unidentifiedAlbums: (limit = 25) =>
    getJson<UnidentifiedAlbum[]>("/api/identification/unidentified", { kind: "album", limit }),
  unidentifiedArtists: (limit = 25) =>
    getJson<UnidentifiedArtist[]>("/api/identification/unidentified", { kind: "artist", limit }),
};

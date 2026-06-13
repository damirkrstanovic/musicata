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
import type { SnapcastStatus } from "../types/SnapcastStatus";
import type { SnapRoomView } from "../types/SnapRoomView";
import type { ArtistAliasGroup } from "../types/ArtistAliasGroup";
import type { Activity } from "../types/Activity";
import type { TrackMetadataReviewResponse } from "../types/TrackMetadataReviewResponse";
import type { TrackMetadataFieldObservation } from "../types/TrackMetadataFieldObservation";
import type { MetadataFieldReviewUpdate } from "../types/MetadataFieldReviewUpdate";
import type { EqProfile } from "./dsp";

// Re-export the generated metadata-review types under the names the editor components use.
export type { MetadataApprovalState } from "../types/MetadataApprovalState";
export type { MetadataFieldValue } from "../types/MetadataFieldValue";
export type MetadataFieldObservation = TrackMetadataFieldObservation;
export type TrackMetadataReview = TrackMetadataReviewResponse;
export type MetadataFieldUpdate = MetadataFieldReviewUpdate;

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
  enrichment: { enriched: number; identified: number; queued: number };
  artwork: { album_covers: number; albums_total: number; artist_images: number; artists_total: number };
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

export interface Playlist {
  id: string;
  name: string;
  comment: string | null;
  song_count: number;
  created_at_unix_seconds: number;
  updated_at_unix_seconds: number;
}

export interface PlaylistDetail extends Playlist {
  tracks: TrackRow[];
}

export interface SmartPlaylist {
  id: string;
  name: string;
  description: string;
}

export interface SmartPlaylistDetail extends SmartPlaylist {
  tracks: TrackRow[];
}

export interface Favorites {
  tracks: TrackRow[];
  albums: Album[];
  artists: Artist[];
}

export type FavoriteKind = "track" | "album" | "artist";

export interface BrowseFacet {
  value: string;
  track_count: number;
}
export interface BrowseYearFacet {
  value: number;
  track_count: number;
}
export interface BrowseIndex {
  genres: BrowseFacet[];
  years: BrowseYearFacet[];
  composers: BrowseFacet[];
  folders: BrowseFacet[];
}

export interface ExportInfo {
  name: string;
  size_bytes: number;
  created_at_unix_seconds: number;
}
export interface ExportStatus {
  running: boolean;
  latest: ExportInfo | null;
  error: string | null;
}

/** Album/track list filters (browse facets), layered onto the paging params. */
export type BrowseParams = ListParams & {
  genre?: string;
  year?: number;
  composer?: string;
};

// Track metadata review types are generated by ts-rs and re-exported at the top of this file.

export type { SourceView, AppSettings, ArtistAliasGroup, Activity, Player, Zone };

export interface CreateSourceRequest {
  kind: "smb" | "opensubsonic";
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

// A 401 means the session expired or never existed — tell the session store (via a DOM event,
// to avoid an import cycle) so the app falls back to the login screen.
function signalIfUnauthorized(status: number) {
  if (status === 401) window.dispatchEvent(new Event("musicata:unauthorized"));
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
  if (!response.ok) {
    signalIfUnauthorized(response.status);
    throw new ApiError(response.status, `GET ${path} → ${response.status}`);
  }
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
    signalIfUnauthorized(response.status);
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
  albums: (params?: BrowseParams) => getJson<Page<Album>>("/api/albums", params),
  tracks: (params?: BrowseParams) => getJson<Page<TrackRow>>("/api/tracks", params),
  browse: () => getJson<BrowseIndex>("/api/browse"),

  // Playlists, smart playlists, favorites
  playlists: () => getJson<Playlist[]>("/api/playlists"),
  playlistDetail: (id: string) => getJson<PlaylistDetail>(`/api/playlists/${encodeURIComponent(id)}`),
  createPlaylist: (name: string) => sendJson<Playlist>("/api/playlists", "POST", { name }),
  addToPlaylist: (id: string, trackIds: string[]) =>
    sendJson(`/api/playlists/${encodeURIComponent(id)}`, "PATCH", { add_track_ids: trackIds }),
  deletePlaylist: (id: string) => sendJson(`/api/playlists/${encodeURIComponent(id)}`, "DELETE"),
  smartPlaylists: () => getJson<SmartPlaylist[]>("/api/smart-playlists"),
  smartPlaylistDetail: (id: string) =>
    getJson<SmartPlaylistDetail>(`/api/smart-playlists/${encodeURIComponent(id)}`),
  favorites: () => getJson<Favorites>("/api/favorites"),
  star: (kind: FavoriteKind, id: string) =>
    sendJson(`/api/favorites/${kind}/${encodeURIComponent(id)}`, "PUT"),
  unstar: (kind: FavoriteKind, id: string) =>
    sendJson(`/api/favorites/${kind}/${encodeURIComponent(id)}`, "DELETE"),

  // Radio
  radio: () => getJson<RadioStation[]>("/api/radio"),

  // Sources
  sources: () => getJson<SourceView[]>("/api/sources"),
  createSource: (body: CreateSourceRequest) => sendJson("/api/sources", "POST", body),
  deleteSource: (id: string) => sendJson(`/api/sources/${encodeURIComponent(id)}`, "DELETE"),
  rescanAll: () => sendJson("/api/sources/all/rescan", "POST"),

  // Settings
  settings: () => getJson<AppSettings>("/api/settings"),
  saveSettings: (body: AppSettings) => sendJson("/api/settings", "PATCH", body),

  // Snapcast multi-room (synchronized playback across rooms)
  snapcastStatus: () => getJson<SnapcastStatus>("/api/snapcast/status"),
  updateSnapcast: (update: {
    enabled?: boolean;
    auth_enabled?: boolean;
    server_host?: string;
    airplay_enabled?: boolean;
    spotify_enabled?: boolean;
    active_input?: string;
    dsp_profile_id?: string;
  }) => sendJson<SnapcastStatus>("/api/snapcast/status", "PATCH", update),
  snapcastRooms: () => getJson<SnapRoomView[]>("/api/snapcast/rooms"),
  addSnapcastRoom: (name: string) =>
    sendJson<SnapRoomView[]>("/api/snapcast/rooms", "POST", { name }),
  deleteSnapcastRoom: (name: string) =>
    sendJson<SnapRoomView[]>(
      `/api/snapcast/rooms/${encodeURIComponent(name)}`,
      "DELETE",
    ),
  setSnapcastVolume: (clientId: string, percent: number) =>
    sendJson(
      `/api/snapcast/clients/${encodeURIComponent(clientId)}/volume`,
      "POST",
      { percent },
    ),

  // Recommendations: radio (seed + similar tracks) + continuous-play toggle.
  trackRadio: (id: string, limit = 25) =>
    getJson<{ track_ids: string[] }>(`/api/tracks/${encodeURIComponent(id)}/radio`, {
      limit: String(limit),
    }),
  autoplay: () => getJson<{ enabled: boolean }>("/api/autoplay"),
  setAutoplay: (enabled: boolean) => sendJson("/api/autoplay", "PUT", { enabled }),

  albumDetail: (id: string) => getJson<AlbumDetail>(`/api/albums/${encodeURIComponent(id)}`),
  artistDetail: (id: string) => getJson<ArtistDetail>(`/api/artists/${encodeURIComponent(id)}`),
  metadataReview: (trackId: string) =>
    getJson<TrackMetadataReview>(`/api/tracks/${encodeURIComponent(trackId)}/metadata/review`),
  updateMetadataField: (trackId: string, update: MetadataFieldUpdate) =>
    sendJson(`/api/tracks/${encodeURIComponent(trackId)}/metadata/review/fields`, "PATCH", update),
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

  // Library export / import (migration)
  exportStatus: () => getJson<ExportStatus>("/api/library/export"),
  startExport: () => sendJson<ExportStatus>("/api/library/export", "POST"),
  importLibrary: async (file: File) => {
    const response = await fetch("/api/library/import", { method: "POST", body: file });
    if (!response.ok) {
      let detail = `${response.status}`;
      try {
        detail = ((await response.json()) as { error?: { message?: string } }).error?.message ?? detail;
      } catch {
        // keep status
      }
      throw new ApiError(response.status, detail);
    }
    return (await response.json()) as { restart_required: boolean };
  },

  // Activity & identification
  activity: () => getJson<Activity[]>("/api/activity"),
  identificationStats: () => getJson<IdentificationStats>("/api/identification/stats"),
  unidentifiedAlbums: (limit = 25) =>
    getJson<UnidentifiedAlbum[]>("/api/identification/unidentified", { kind: "album", limit }),
  unidentifiedArtists: (limit = 25) =>
    getJson<UnidentifiedArtist[]>("/api/identification/unidentified", { kind: "artist", limit }),

  // DSP profile library (server-synced EQ / room correction)
  dspProfiles: () => getJson<EqProfile[]>("/api/dsp/profiles"),
  saveDspProfile: (p: EqProfile) =>
    sendJson<EqProfile>(`/api/dsp/profiles/${encodeURIComponent(p.id)}`, "PUT", p),
  deleteDspProfile: (id: string) => sendJson(`/api/dsp/profiles/${encodeURIComponent(id)}`, "DELETE"),
  uploadRoomIr: async (id: string, wav: ArrayBuffer): Promise<void> => {
    const r = await fetch(`/api/dsp/profiles/${encodeURIComponent(id)}/impulse`, {
      method: "POST",
      body: wav,
    });
    if (!r.ok) {
      signalIfUnauthorized(r.status);
      throw new ApiError(r.status, `upload impulse → ${r.status}`);
    }
  },
  deleteRoomIr: (id: string) =>
    sendJson(`/api/dsp/profiles/${encodeURIComponent(id)}/impulse`, "DELETE"),

  // Auth & users
  authStatus: () => getJson<{ setup_required: boolean }>("/api/auth/status"),
  me: () => getJson<SessionUser>("/api/auth/me"),
  login: (username: string, password: string) =>
    sendJson<{ user: SessionUser }>("/api/auth/login", "POST", { username, password }),
  setup: (username: string, password: string) =>
    sendJson<{ user: SessionUser }>("/api/auth/setup", "POST", { username, password }),
  logout: () => sendJson("/api/auth/logout", "POST"),
  changePassword: (current_password: string, new_password: string) =>
    sendJson("/api/auth/password", "POST", { current_password, new_password }),
  apiToken: () => getJson<{ api_token: string }>("/api/auth/token"),
  rotateApiToken: () => sendJson<{ api_token: string }>("/api/auth/token", "POST"),
  listUsers: () => getJson<{ users: SessionUser[] }>("/api/users"),
  createUser: (username: string, password: string, role: string) =>
    sendJson<SessionUser>("/api/users", "POST", { username, password, role }),
  updateUser: (id: string, patch: { role?: string; password?: string }) =>
    sendJson(`/api/users/${encodeURIComponent(id)}`, "PATCH", patch),
  deleteUser: (id: string) => sendJson(`/api/users/${encodeURIComponent(id)}`, "DELETE"),
};

/** The authenticated user (matches the server's UserView). */
export interface SessionUser {
  id: string;
  username: string;
  role: string;
}

// SPDX-License-Identifier: AGPL-3.0-or-later
export const SETTINGS_CATEGORIES = [
  {id: "sources", label: "Music sources"},
  {id: "outputs", label: "Outputs & groups"},
  {id: "sound", label: "Sound profiles"},
  {id: "metadata", label: "Metadata & artwork"},
  {id: "history", label: "History & services"},
  {id: "system", label: "System & accounts"},
] as const;
export type SettingsCategory = typeof SETTINGS_CATEGORIES[number]["id"];
// Search destinations, never setting values (which may contain credentials).
export const SETTINGS_DESTINATIONS: {label: string; category: SettingsCategory; keywords: string}[] = [
  {label: "Music sources", category: "sources", keywords: "folder library network share smb provider connection"},
  {label: "Import & export", category: "sources", keywords: "backup restore playlists"},
  {label: "Outputs & groups", category: "outputs", keywords: "player zone device MPD native browser"},
  {label: "Synchronized playback", category: "outputs", keywords: "Snapcast server clients transport"},
  {label: "EQ profiles & room correction", category: "sound", keywords: "equalizer headphones speakers AutoEq preset impulse response import"},
  {label: "Artwork & identification", category: "metadata", keywords: "MusicBrainz AcoustID fingerprint fanart cover metadata"},
  {label: "Artist merging", category: "metadata", keywords: "alias curation duplicate artists"},
  {label: "Audio similarity analysis", category: "history", keywords: "sounds-like radio mix recommendations ML schedule"},
  {label: "Listening history", category: "history", keywords: "record listens privacy clear"},
  {label: "Scrobbling", category: "history", keywords: "ListenBrainz token services"},
  {label: "Status & diagnostics", category: "system", keywords: "errors activity logs debug support download"},
  {label: "Accounts", category: "system", keywords: "users password access permission"},
  {label: "About & source", category: "system", keywords: "version license source code"},
];

// SPDX-License-Identifier: AGPL-3.0-or-later
// AutoEq headphone correction presets, fetched on demand through Musicata's own API.
//
// Deliberately NOT bundled. AutoEq's own code is MIT, but its published results are computed
// from headphone measurements contributed by third parties (oratory1990, Crinacle, Rtings and
// others), each publishing under its own terms — some non-commercial. Musicata stores nothing:
// the server relays each response straight through (see crate::proxy) and a preset only ever
// comes to rest in the user's own saved profile. See NOTICE.
//
// The page cannot reach raw.githubusercontent.com itself — the CSP allows this origin only —
// so both requests go to the server, which fetches upstream. That also keeps the user's IP
// from reaching a third party the operator never configured. `path` is sent exactly as
// AutoEq's INDEX.md spells it; the server derives the filename and rejects anything that would
// escape the results/ base (see crate::autoeq).

const API = "/api/autoeq";

export interface AutoEqEntry {
  /** Headphone model, as AutoEq names it — e.g. "Sennheiser HD 600". */
  name: string;
  /** Who measured it. Several sources publish the same model, with different results. */
  source: string;
  /** Measurement rig, when the source publishes on more than one. */
  rig?: string;
  /** Path under `results/`, percent-encoded as it appears in the index. */
  path: string;
}

/**
 * Parse AutoEq's `results/INDEX.md`. Lines look like:
 *
 *     - [Sennheiser HD 600](./oratory1990/over-ear/Sennheiser%20HD%20600) by oratory1990
 *     - [1MORE Aero (ANC Off)](./DHRME/in-ear/1MORE%20Aero%20(ANC%20Off)) by DHRME
 *     - [Sennheiser HD 600](./crinacle/GRAS%2043AG-7%20over-ear/…) by crinacle on GRAS 43AG-7
 *
 * Model names contain parentheses, so the link group is matched **greedily** and anchored on
 * the trailing `) by …` — a lazy match would stop inside `(ANC Off)` and truncate the path.
 * Unparseable lines (the file's own headings and prose) are skipped.
 */
export function parseIndex(markdown: string): AutoEqEntry[] {
  const entries: AutoEqEntry[] = [];
  for (const raw of markdown.split(/\r?\n/)) {
    const m = raw.match(/^- \[(.+)\]\(\.\/(.+)\) by (.+)$/);
    if (!m) continue;
    const [, name, path, by] = m;
    const on = by.indexOf(" on ");
    entries.push({
      name,
      path,
      source: on === -1 ? by : by.slice(0, on),
      rig: on === -1 ? undefined : by.slice(on + 4),
    });
  }
  return entries;
}

/** Models whose name contains every whitespace-separated term, prefix matches first. */
export function searchIndex(entries: AutoEqEntry[], query: string, limit = 10): AutoEqEntry[] {
  const terms = query.trim().toLowerCase().split(/\s+/).filter(Boolean);
  if (terms.length === 0) return [];
  const matches = entries.filter((e) => {
    const name = e.name.toLowerCase();
    return terms.every((t) => name.includes(t));
  });
  // A search for "hd 600" should surface "Sennheiser HD 600" ahead of "Sennheiser HD 600 (foo)",
  // and both ahead of a model that merely contains the terms somewhere in the middle.
  const rank = (e: AutoEqEntry) => {
    const name = e.name.toLowerCase();
    return (name.startsWith(terms[0]) ? 0 : 1) * 1000 + name.length;
  };
  return matches.sort((a, b) => rank(a) - rank(b) || a.name.localeCompare(b.name)).slice(0, limit);
}

/**
 * Where to ask the server for an entry's ParametricEQ file. `entry.path` is already
 * percent-encoded (it comes verbatim out of INDEX.md), so encoding it again as a query value
 * is correct — the server decodes exactly once and gets the path back as AutoEq spells it.
 */
export function presetUrl(entry: AutoEqEntry): string {
  return `${API}/preset?path=${encodeURIComponent(entry.path)}`;
}

// The index is ~850 KB (~110 KB over the wire) and changes rarely, so it is fetched at most
// once per page load. The promise — not the result — is memoized so concurrent callers share
// one request; a failed fetch clears it so the next attempt retries instead of caching the error.
let pending: Promise<AutoEqEntry[]> | null = null;

export function loadIndex(): Promise<AutoEqEntry[]> {
  pending ??= fetch(`${API}/index`)
    .then((res) => {
      if (!res.ok) throw new Error(`AutoEq index: HTTP ${res.status}`);
      return res.text();
    })
    .then(parseIndex)
    .catch((err) => {
      pending = null;
      throw err;
    });
  return pending;
}

export async function fetchPreset(entry: AutoEqEntry): Promise<string> {
  const res = await fetch(presetUrl(entry));
  if (!res.ok) throw new Error(`AutoEq preset: HTTP ${res.status}`);
  return res.text();
}

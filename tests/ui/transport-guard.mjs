// Source guard: every playback-affecting command (play/next/previous/play_tracks/play_stream)
// must be issued only from the transport module (lib/playback.ts), which claims + primes the
// browser output inside the user gesture. A raw command sent from anywhere else leaves drive() a
// no-op (the tab never claimed output) — the UI reads "playing" over silence. This catches that
// at author time, no browser needed. Runs from scripts/v2-smoke.sh and standalone:
//   node tests/ui/transport-guard.mjs
import { readdirSync, readFileSync, statSync } from "node:fs";
import { fileURLToPath } from "node:url";

const SRC = fileURLToPath(new URL("../../crates/musicata-server/web/src", import.meta.url));
// The transport module itself (legitimate sends) + the command type definitions.
const ALLOW = new Set(["lib/playback.ts", "lib/commands.ts"]);
const FORBIDDEN = /command:\s*"(play|next|previous|play_tracks|play_stream)"/;

function walk(dir) {
  const out = [];
  for (const name of readdirSync(dir)) {
    const p = `${dir}/${name}`;
    if (statSync(p).isDirectory()) out.push(...walk(p));
    else if (/\.(ts|svelte)$/.test(name)) out.push(p);
  }
  return out;
}

const violations = [];
for (const file of walk(SRC)) {
  const rel = file.slice(SRC.length + 1);
  if (ALLOW.has(rel)) continue;
  readFileSync(file, "utf8")
    .split("\n")
    .forEach((line, i) => {
      if (FORBIDDEN.test(line)) violations.push(`${rel}:${i + 1}: ${line.trim()}`);
    });
}

if (violations.length) {
  console.error("✗ transport-guard: raw playback command outside lib/playback.ts:");
  for (const v of violations) console.error("  " + v);
  console.error(
    "\nRoute it through a transport verb in lib/playback.ts (pause/resume/togglePlayback/next/" +
      "previous/playTracks/playStream) so the browser output is claimed + primed in the gesture.",
  );
  process.exit(1);
}
console.log("✓ transport-guard: all playback commands route through lib/playback.ts");

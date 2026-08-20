// Regenerate THIRD-PARTY-NOTICES.md — the attribution that must ride along with every
// Musicata *binary* (release tarball, container image), as distinct from NOTICE, which
// covers third-party material stored in this repository.
//
// Why it exists: the server binary statically links the whole Rust dependency tree and
// embeds `web/dist` (rust-embed), whose minified bundle carries the Svelte runtime and
// Workbox with their copyright banners stripped. MIT/BSD/ISC require the notice to travel
// with "all copies"; Apache-2.0 §4(d) requires propagating upstream NOTICE files; MPL-2.0
// §3.2 requires telling binary recipients how to get the MPL source.
//
// Run after changing dependencies (Cargo.lock or the web package-lock):
//   npm --prefix crates/musicata-server/web run build   # dist/ must exist, see below
//   node scripts/gen-third-party-notices.mjs
//
// Rust crates come from Cargo.lock resolved against the local registry cache, so run
// `cargo fetch` first on a clean machine — the script fails loudly rather than silently
// omitting a crate. The npm side is deliberately narrow: only packages whose code actually
// lands in `web/dist` are redistributed (build-time tooling is not), and the shipped
// Workbox modules are read out of the built bundle's own `workbox:<pkg>:<version>` markers
// so the list can't drift from what we ship.
import { createHash } from "node:crypto";
import { existsSync, readdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { fileURLToPath } from "node:url";

const ROOT = fileURLToPath(new URL("..", import.meta.url));
const WEB = `${ROOT}crates/musicata-server/web`;
const OUT = `${ROOT}THIRD-PARTY-NOTICES.md`;

const CARGO_HOME = process.env.CARGO_HOME || `${homedir()}/.cargo`;
const REGISTRY = `${CARGO_HOME}/registry/src`;

// Files a crate/package ships its terms in. NOTICE is included because Apache-2.0 §4(d)
// makes propagating it mandatory.
const LICENSE_FILE = /^(licen[cs]e|copying|notice)([-_.].*)?$/i;
// A license file should be a license, not a vendored corpus; anything this large is probably a
// mispackaged data blob. Exceeding it is a hard failure, not a skip — see readLicenseFiles.
const MAX_LICENSE_BYTES = 64 * 1024;

function fail(message) {
  console.error(`gen-third-party-notices: ${message}`);
  process.exit(1);
}

function readLicenseFiles(dir, oversized) {
  const out = [];
  for (const name of readdirSync(dir)) {
    if (!LICENSE_FILE.test(name)) continue;
    const path = `${dir}/${name}`;
    if (!statSync(path).isFile()) continue;
    // Never skip a license quietly — dropping one is exactly the under-attribution this file
    // exists to prevent. Oversized files are collected and reported by the caller, which fails.
    if (statSync(path).size > MAX_LICENSE_BYTES) {
      oversized.push(path);
      continue;
    }
    const text = readFileSync(path, "utf8").trim();
    if (text) out.push({ name, text });
  }
  return out.sort((a, b) => a.name.localeCompare(b.name));
}

// ---- Rust: every package in Cargo.lock, minus this workspace's own crates ----------------

function rustPackages(oversized) {
  const lock = readFileSync(`${ROOT}Cargo.lock`, "utf8");
  const own = new Set(
    readdirSync(`${ROOT}crates`).filter((n) => existsSync(`${ROOT}crates/${n}/Cargo.toml`)),
  );
  // Cache the registry's per-source directories once; each holds `<name>-<version>/`.
  const sources = existsSync(REGISTRY)
    ? readdirSync(REGISTRY).map((d) => `${REGISTRY}/${d}`)
    : fail(`no cargo registry cache at ${REGISTRY} — run \`cargo fetch\` first`);

  const packages = [];
  const missing = [];
  for (const block of lock.split("[[package]]").slice(1)) {
    const name = block.match(/^\s*name = "([^"]+)"/m)?.[1];
    const version = block.match(/^\s*version = "([^"]+)"/m)?.[1];
    if (!name || !version || own.has(name)) continue;

    const dir = sources.map((s) => `${s}/${name}-${version}`).find((p) => existsSync(p));
    if (!dir) {
      missing.push(`${name} ${version}`);
      continue;
    }
    const manifest = readFileSync(`${dir}/Cargo.toml`, "utf8");
    const license =
      manifest.match(/^license\s*=\s*"([^"]+)"/m)?.[1] ??
      (manifest.match(/^license-file\s*=\s*"([^"]+)"/m) ? "see license file" : null);
    if (!license) fail(`${name} ${version} declares no license — resolve before releasing`);
    packages.push({
      name,
      version,
      license,
      files: readLicenseFiles(dir, oversized),
      ecosystem: "rust",
    });
  }

  if (missing.length) {
    fail(
      `${missing.length} crate(s) not in the registry cache — run \`cargo fetch\` (and ` +
        `\`cargo fetch\` for the non-default members): ${missing.slice(0, 5).join(", ")}` +
        (missing.length > 5 ? ", …" : ""),
    );
  }
  return packages.sort((a, b) => a.name.localeCompare(b.name));
}

// ---- npm: only what actually ends up inside web/dist -------------------------------------

function shippedNpmPackages(oversized) {
  const dist = `${WEB}/dist`;
  if (!existsSync(dist)) {
    fail(`${dist} missing — run \`npm --prefix crates/musicata-server/web run build\` first`);
  }

  // Workbox stamps `self["workbox:<pkg>:<version>"]` into each module it bundles, so the
  // built service worker is the authoritative list of which Workbox packages we ship.
  const names = new Set(["svelte"]);
  const walk = (dir) => {
    for (const entry of readdirSync(dir)) {
      const path = `${dir}/${entry}`;
      if (statSync(path).isDirectory()) walk(path);
      else if (/\.(js|css)$/.test(entry)) {
        for (const [, pkg] of readFileSync(path, "utf8").matchAll(/workbox:([a-z-]+):[\d.]+/g)) {
          names.add(`workbox-${pkg}`);
        }
      }
    }
  };
  walk(dist);

  return [...names].sort().map((name) => {
    const dir = `${WEB}/node_modules/${name}`;
    if (!existsSync(dir)) fail(`${name} is in the bundle but not installed — run \`npm ci\``);
    const pkg = JSON.parse(readFileSync(`${dir}/package.json`, "utf8"));
    const license =
      typeof pkg.license === "string" ? pkg.license : (pkg.license?.type ?? pkg.licenses?.[0]?.type);
    if (!license) fail(`${name} declares no license — resolve before releasing`);
    return {
      name,
      version: pkg.version,
      license,
      files: readLicenseFiles(dir, oversized),
      ecosystem: "npm",
    };
  });
}

// ---- Render -------------------------------------------------------------------------------

function inventory(title, packages) {
  const rows = packages.map((p) => `| ${p.name} | ${p.version} | ${p.license} |`);
  return [`### ${title}\n`, "| Package | Version | License |", "| --- | --- | --- |", ...rows].join(
    "\n",
  );
}

// One copy of each distinct license text, listing every package that ships it. Deduplicating
// on the text itself keeps each package's own copyright line (which is what MIT and BSD
// actually require) while collapsing the identical Apache-2.0 boilerplate into one copy.
function licenseTexts(packages) {
  const byText = new Map();
  for (const pkg of packages) {
    for (const file of pkg.files) {
      const key = createHash("sha256").update(file.text).digest("hex");
      const entry = byText.get(key) ?? { text: file.text, users: [] };
      entry.users.push(`${pkg.name} ${pkg.version}`);
      byText.set(key, entry);
    }
  }
  return [...byText.values()]
    .sort((a, b) => b.users.length - a.users.length || a.text.localeCompare(b.text))
    .map(({ text, users }) => {
      const heading = users.length === 1 ? users[0] : `${users.length} packages`;
      const list = users.length === 1 ? "" : `\n${users.map((u) => `- ${u}`).join("\n")}\n`;
      return `### ${heading}\n${list}\n\`\`\`\n${text}\n\`\`\`\n`;
    })
    .join("\n");
}

// A license file too large to inline is almost always a mispackaged data blob, but it might
// be a real license — so stop and make a human look, rather than shipping without it.
const oversized = [];
const rust = rustPackages(oversized);
const npm = shippedNpmPackages(oversized);
if (oversized.length) {
  fail(
    `${oversized.length} license file(s) exceed ${MAX_LICENSE_BYTES} bytes and were not ` +
      `inlined. Check each one: if it is a real license, raise MAX_LICENSE_BYTES; if it is a ` +
      `mispackaged data blob, add an explicit exception. Do not ignore this.\n  ` +
      oversized.join("\n  "),
  );
}
const all = [...rust, ...npm];
const noText = all.filter((p) => p.files.length === 0);

const doc = `# Third-party notices

Musicata is licensed under the GNU Affero General Public License v3.0 or later; see
[COPYING](COPYING) for its terms and [NOTICE](NOTICE) for third-party material stored in this
repository. **This file covers something different:** the third-party code that is compiled
*into* a Musicata binary and therefore redistributed with every release tarball and container
image.

The server binary statically links its Rust dependency tree and embeds the built web app
(\`web/dist\`), whose minified bundle contains the Svelte runtime and Workbox with their
copyright banners removed. The notices below restore that attribution. Regenerate with
\`node scripts/gen-third-party-notices.mjs\` after changing dependencies.

The Rust inventory is the union of every crate in \`Cargo.lock\`, which spans the whole
workspace — a given binary links a subset (\`musicata-server\` never links the \`musicata-ml\`
or \`musicata-endpoint\` trees). Over-listing is deliberate: it cannot under-attribute. The
npm inventory is the opposite — narrowed to the packages whose code actually reaches
\`web/dist\`, because build-time tooling is never redistributed.

## Obtaining source

Corresponding Source for Musicata itself, and for the modifications (if any) in the build you
received, is available at <https://github.com/damirkrstanovic/musicata>. Each release is
tagged, and each container image carries its commit in the
\`org.opencontainers.image.revision\` label.

Several dependencies are licensed under the Mozilla Public License 2.0 — the Symphonia audio
decoders. MPL-2.0 §3.2 entitles you to their source in the form used to build this binary;
the exact versions are listed below and are available from
<https://crates.io> and <https://github.com/pdeljanov/Symphonia>.

## Inventory

${inventory(`Rust crates (${rust.length})`, rust)}

${inventory(`npm packages redistributed in the web bundle (${npm.length})`, npm)}
${
  noText.length
    ? `\n> ${noText.length} package(s) ship no license file in their published artifact; ` +
      `their SPDX expression above is the operative grant: ` +
      `${noText.map((p) => `${p.name} ${p.version}`).join(", ")}.\n`
    : ""
}
## License texts

${licenseTexts(all)}`;

writeFileSync(OUT, doc);
console.log(
  `Wrote THIRD-PARTY-NOTICES.md — ${rust.length} crates, ${npm.length} npm packages, ` +
    `${(Buffer.byteLength(doc) / 1024).toFixed(0)} KB`,
);

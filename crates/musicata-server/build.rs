// SPDX-License-Identifier: AGPL-3.0-or-later
//! Builds the embedded web app (Svelte + Vite, under `web/`) during `cargo build`, so the
//! server binary always carries freshly-compiled, matching assets — there is no separate
//! frontend build step to forget. The output lands in `web/dist/`, which `main.rs` embeds
//! via `rust-embed`.
//!
//! Set `MUSICATA_SKIP_WEB_BUILD=1` to skip the npm build and reuse an existing `web/dist/`
//! (for CI / offline machines without Node). The build only re-runs when `web/` sources
//! change.
//!
//! Also stamps the build's commit into `MUSICATA_GIT_SHA` for `--version` and the AGPL
//! section 13 source offer, so a user can tell which source their instance was built from.

use std::path::Path;
use std::process::Command;

fn main() {
    stamp_git_sha();

    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let web = Path::new(&manifest).join("web");
    let dist = web.join("dist");

    // Re-run only when the frontend sources change (not on every unrelated Rust edit).
    for entry in [
        "src",
        "index.html",
        "admin.html",
        "package.json",
        "package-lock.json",
        "vite.config.ts",
        "svelte.config.js",
        "tsconfig.json",
    ] {
        println!("cargo:rerun-if-changed={}", web.join(entry).display());
    }
    println!("cargo:rerun-if-env-changed=MUSICATA_SKIP_WEB_BUILD");

    // rust-embed needs the folder to exist at macro-expansion time, even when empty.
    std::fs::create_dir_all(&dist).expect("create web/dist");

    if std::env::var_os("MUSICATA_SKIP_WEB_BUILD").is_some() {
        if !dist.join("index.html").exists() {
            println!(
                "cargo:warning=MUSICATA_SKIP_WEB_BUILD set but web/dist has no build; the web UI will be unavailable"
            );
        }
        return;
    }

    let npm = if cfg!(windows) { "npm.cmd" } else { "npm" };
    if !web.join("node_modules").exists() {
        run(npm, &["ci"], &web);
    }
    run(npm, &["run", "build"], &web);
}

/// Expose the commit this binary was built from as `MUSICATA_GIT_SHA`. An explicitly set
/// env var wins (container builds have no `.git` — `.dockerignore` excludes it — so CI can
/// inject the revision it checked out); otherwise ask git; otherwise leave it empty and let
/// `--version` report the crate version alone. Never fails the build: an unknown commit is a
/// missing nicety, not a broken binary.
fn stamp_git_sha() {
    println!("cargo:rerun-if-env-changed=MUSICATA_GIT_SHA");
    println!("cargo:rerun-if-changed=../../.git/HEAD");

    let sha = std::env::var("MUSICATA_GIT_SHA").ok().unwrap_or_else(|| {
        Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .output()
            .ok()
            .filter(|out| out.status.success())
            .and_then(|out| String::from_utf8(out.stdout).ok())
            .unwrap_or_default()
    });
    println!("cargo:rustc-env=MUSICATA_GIT_SHA={}", sha.trim());
}

fn run(cmd: &str, args: &[&str], dir: &Path) {
    match Command::new(cmd).args(args).current_dir(dir).status() {
        Ok(status) if status.success() => {}
        Ok(status) => panic!("`{cmd} {}` failed ({status})", args.join(" ")),
        Err(error) => panic!(
            "could not run `{cmd} {}` in {}: {error}.\n\
             Install Node + npm, or set MUSICATA_SKIP_WEB_BUILD=1 with a prebuilt web/dist/.",
            args.join(" "),
            dir.display()
        ),
    }
}

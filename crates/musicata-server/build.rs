//! Builds the embedded web app (Svelte + Vite, under `web/`) during `cargo build`, so the
//! server binary always carries freshly-compiled, matching assets — there is no separate
//! frontend build step to forget. The output lands in `web/dist/`, which `main.rs` embeds
//! via `rust-embed`.
//!
//! Set `MUSICATA_SKIP_WEB_BUILD=1` to skip the npm build and reuse an existing `web/dist/`
//! (for CI / offline machines without Node). The build only re-runs when `web/` sources
//! change.

use std::path::Path;
use std::process::Command;

fn main() {
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

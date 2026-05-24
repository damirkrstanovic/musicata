# Repository Guidelines

## Project Structure & Module Organization

This repository is a Rust workspace for Musicata, an open source music server/controller prototype. Keep source organized by crate and keep provider-neutral logic out of server-specific code.

- `crates/musicata-core/` contains domain types, provider traits, and local library scanning.
- `crates/musicata-server/` contains the HTTP API, static web controller, and playback streaming routes.
- `crates/musicata-server/static/` contains the current PWA-style web assets.
- `docs/` contains research and requirements.
- `testdata/` contains sample music used by scanner tests and manual flow testing.

Avoid adding source files at the repository root unless they are standard project entry points.

## Build, Test, and Development Commands

Use Cargo from the repository root:

- `cargo run -p musicata-server` starts the prototype server at `127.0.0.1:3030` and scans `testdata`.
- `cargo run -p musicata-server -- --library /path/to/music --addr 127.0.0.1:3031` runs against another library or port.
- `cargo run -p musicata-server -- --config musicata.example.conf` runs with a config file; environment overrides use `MUSICATA_LIBRARY`, `MUSICATA_DATABASE`, `MUSICATA_ADDR`, and `MUSICATA_RESCAN`.
- `cargo test --offline` runs all unit tests without fetching dependencies.
- `cargo fmt --all` formats the workspace.
- `cargo check --offline` verifies the workspace quickly.

The current server uses Axum, Tokio, Serde, and `tracing`. In restricted environments with a read-only global Cargo registry, use a writable `CARGO_HOME`.

## Coding Style & Naming Conventions

Use Rust 2024 edition and standard `rustfmt` output. Keep modules small, explicit, and aligned with the architecture: providers belong in core, transport concerns belong in server. Use `snake_case` for functions/modules, `PascalCase` for types and traits, and `SCREAMING_SNAKE_CASE` only for true constants.

For web assets, use plain `kebab-case` filenames and keep JavaScript as progressive controller glue until the Rust web stack is introduced.

## Testing Guidelines

Prefer unit tests beside the Rust code they cover. Scanner tests currently use `testdata/`, so keep that sample library stable enough for deterministic counts and searches. Do not require private credentials or network access in default tests.

Each feature should cover expected behavior and at least one failure or edge case when applicable. Run `cargo test --offline` before handing off changes.

## Commit & Pull Request Guidelines

No usable Git history is available in this workspace, so there is no existing commit convention to preserve. Use short, imperative commit subjects, for example `Add playback queue model`. Conventional Commit prefixes such as `feat:`, `fix:`, and `docs:` are acceptable if adopted consistently.

Pull requests should include a concise summary, verification commands, linked issues when relevant, and screenshots or recordings for UI changes.

## Agent-Specific Instructions

Keep generated changes scoped to the requested task. Do not collapse provider-neutral domain logic into local-disk or browser-specific code. Preserve the Rust-first direction and keep dependency additions tied to roadmap milestones.

Coding Principles
1. Think Before Coding

Don't assume. Don't hide confusion. Surface tradeoffs.

    State your assumptions explicitly. If uncertain, ask.
    If multiple interpretations exist, present them - don't pick silently.
    If a simpler approach exists, say so. Push back when warranted.
    If something is unclear, stop. Name what's confusing. Ask.

2. Simplicity First

Minimum code that solves the problem. Nothing speculative.

    No features beyond what was asked.
    No abstractions for single-use code.
    No "flexibility" or "configurability" that wasn't requested.
    No error handling for impossible scenarios.
    If you write 200 lines and it could be 50, rewrite it.

Ask yourself: "Would a senior engineer say this is overcomplicated?" If yes, simplify.
3. Surgical Changes

Touch only what you must. Clean up only your own mess.

When editing existing code:

    Don't "improve" adjacent code, comments, or formatting.
    Don't refactor things that aren't broken.
    Match existing style, even if you'd do it differently.
    If you notice unrelated dead code, mention it - don't delete it.

When your changes create orphans:

    Remove imports/variables/functions that YOUR changes made unused.
    Don't remove pre-existing dead code unless asked.

The test: every changed line should trace directly to the user's request.
4. Goal-Driven Execution

Define success criteria. Loop until verified.

Transform tasks into verifiable goals:

    "Add validation" → "Write tests for invalid inputs, then make them pass"
    "Fix the bug" → "Write a test that reproduces it, then make it pass"
    "Refactor X" → "Ensure tests pass before and after"

For multi-step tasks, state a brief plan:

    [Step] → verify: [check]
    [Step] → verify: [check]
    [Step] → verify: [check]

Strong success criteria let you loop independently. Weak criteria ("make it work") require constant clarification.

These principles are working if: fewer unnecessary changes in diffs, fewer rewrites due to overcomplication, and clarifying questions come before implementation rather than after mistakes.

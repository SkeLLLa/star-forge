# star-forge — agent instructions

Instructions for AI coding agents that make commits in this repository. Human contributors
can ignore this file; see `README.md` and `docs/` instead.

Rust 2024 binary crate. MSRV 1.88 (`clippy.toml`, `Cargo.toml`).

## Setup

`mise install` installs the toolchain plus the fff and ripwire MCP servers (see `.mcp.json`) and
mr-boxington (`mbx`), which routes plain `cargo` through a shared compile cache.

## Layout

Single `star-forge` crate: the `stfgd` daemon and the `stfg` bin (std + libc `get` client, in
`src/client.rs`). The `daemon` feature (default) gates the daemon's dependencies; `cargo build
--no-default-features --bin stfg` compiles only `libc`.

## Commands

- `mise run check` — fmt-check, clippy (`-D warnings`), tests, docs, Markdown lint
  (`rumdl`). Run before declaring work done.
- `mise run fmt` / `md-fmt` / `lint` / `md-check` / `test` / `build`.

## Conventions

- Clippy `pedantic`, `nursery`, `cargo` are on; fix warnings, don't `allow` them locally without
  a reason.
- Commits and PR titles follow Conventional Commits (`<type>(<scope>)!?: <description>`); CI
  enforces it.
- Search files and content with fff MCP; navigate symbols, check impact, and recall project
  memory (`docs/memory/`) with ripwire.
- Prefix shell commands with `rtk`.

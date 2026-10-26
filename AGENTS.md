# star-forge — agent instructions

Instructions for AI coding agents that make commits in this repository. Human contributors
can ignore this file; see `README.md` and `docs/` instead.

Rust 2024 binary crate. MSRV 1.88 (`clippy.toml`, `Cargo.toml`).

## Setup

`mise install` installs the toolchain plus the fff and ripwire MCP servers (see `.mcp.json`) and
mr-boxington (`mbx`), a Cargo wrapper with a shared compile cache. The mise tasks that compile
call `mbx` directly; plain `cargo` bypasses the cache. Prefer the mise tasks, or prefix ad-hoc
Cargo commands with `mbx` (`mbx build --no-default-features --bin stfg`). CI uses plain Cargo.

## Layout

Single `star-forge` crate: the `stfgd` daemon and the `stfg` bin (std + libc `get` client, in
`src/client.rs`). The `daemon` feature (default) gates the daemon's dependencies; `cargo build
--no-default-features --bin stfg` compiles only `libc`.

## Commands

- `mise run check` — fmt-check, clippy (`-D warnings`), tests, docs, Markdown lint
  (`rumdl`), action pins, jactionlint and shellcheck. Run before declaring work done.
- `mise run fmt` / `md-fmt` / `lint` / `md-check` / `test` / `build` / `build-release` / `bench`.

## Conventions

- Clippy `pedantic`, `nursery`, `cargo` are on; fix warnings, don't `allow` them locally without
  a reason.
- Commits and PR titles follow Conventional Commits (`<type>(<scope>)!?: <description>`); CI
  enforces it.
- Search files and content with fff MCP; navigate symbols, check impact, and recall project
  memory (`docs/memory/`) with ripwire.
- Workflows: pin every `uses:` to a full commit SHA with a `# vX.Y.Z` comment, keep
  `permissions` minimal per job and set `timeout-minutes`; see `docs/distribution.md#hardening`.
- Prefix shell commands with `rtk`.

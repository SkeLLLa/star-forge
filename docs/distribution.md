# Distribution

## Goals

- Produce standalone binary archives for users without distro packaging.
- Produce native `deb` and `rpm` packages for system-wide installation.
- Publish static RPM and APT repositories through GitHub Pages.
- Ship a systemd user unit with the native packages so the daemon can start with the session.
- Keep `cargo install` working with no extra files: a missing config is an empty config and the
  daemon is spawned on first `get`.

## Artifact Layout

### Binary archives

Linux (`x86_64-unknown-linux-gnu`, `x86_64-unknown-linux-musl`) and macOS (`aarch64-apple-darwin`, `x86_64-apple-darwin`)
archives contain:

- `bin/stfgd`
- `bin/stfg`
- `README.md`
- `COPYING`
- `THIRD_PARTY_NOTICES.md`

The `stfg` in the gnu tarball, `deb` and `rpm` is itself a static musl build (the release job
builds it with `--no-default-features --bin stfg --target x86_64-unknown-linux-musl` right after
the main build and installs it over the gnu one), so it never loads libc. The
`x86_64-unknown-linux-musl` archive is the same layout with statically linked binaries (no
libc or `libgcc_s` to load). It is published only as a `tar.gz`; there is no `deb`/`rpm`. Choose it
when you want the fastest `stfg` startup (a dynamic binary spends roughly 200 µs per call on
loading and runtime init), or on a distro without a compatible glibc (Alpine, old LTS). Otherwise
prefer the `gnu` archive or a native package. musl's allocator is slower than glibc's: `stfg`
barely allocates, but the `stfgd` daemon (tokio) may be marginally slower under load. The musl
archive is in the packslip manifest beside the `gnu` one; packslip and `mise` pick between them by
the host's libc.

### `deb` / `rpm`

Native packages install:

- `/usr/bin/stfgd`
- `/usr/bin/stfg`
- `/usr/lib/systemd/user/star-forge.service`
- `/usr/share/doc/star-forge/README.md`
- `/usr/share/doc/star-forge/COPYING`
- `/usr/share/doc/star-forge/THIRD_PARTY_NOTICES.md`

The unit source is `packaging/systemd/star-forge.service`. It runs `/usr/bin/stfgd daemon`, maps
`systemctl --user reload` to `stfgd reload`, restarts only on failure, and is wanted by
`default.target`. Packages never enable it; each user opts in with `systemctl --user`.

The unit does not change the daemon's lifecycle:

- `daemon.idle_exit` (default 30m) still applies. The idle exit is clean, so the unit goes
  inactive and the next `get` spawns a daemon outside systemd, exactly as without the unit. Set
  `idle_exit` to a long duration (e.g. `"8760h"`) to keep the unit running for the session.
- The daemon is a per-user `flock` singleton. If one is already running (spawned by an earlier
  `get`), the unit's daemon exits cleanly at once; run `stfgd stop`, then start the unit.
- `systemctl --user stop` sends `SIGTERM`, which the daemon handles like `stfgd stop`.

### GitHub Pages repositories

Release CI publishes unsigned package repositories at:

- `https://skellla.github.io/star-forge/rpm/x86_64`
- `https://skellla.github.io/star-forge/deb`

The RPM repository is generated with `createrepo_c --compatibility`. The APT repository uses a
`stable` suite and `main` component generated from the published `.deb` packages. GitHub Releases
remain the long-term artifact archive; Pages keeps the current release plus up to nine previous
ones.

One-time repository setup: GitHub Pages must use **GitHub Actions** as its source (Settings →
Pages → Build and deployment), or equivalently:

```bash
gh api -X POST repos/SkeLLLa/star-forge/pages -f build_type=workflow
```

The `github-pages` environment is created by the first deployment and must allow deployments
from `master`.

### `cargo install`

`cargo install` only places `stfgd` and `stfg` in Cargo's bin directory. No setup is needed; to
use the user unit anyway, install a copy pointing at the Cargo binary (see First-Run Flow below).

## CI Plan

GitHub Actions keeps source verification separate from release automation:

- `ci.yml`: Conventional Commit check, action pin check, workflow lint (jactionlint with
  shellcheck), stable quality gate, MSRV build, musl and Nix builds, macOS builds (with an ad-hoc
  run of the signing script), and `cargo audit` on pushes and pull requests.
- `release.yml`: update the version and `CHANGELOG.md` directly on `master`, then create the tag,
  publish the crate, create the GitHub release, build release artifacts, attach them, and publish
  the package repositories.

### Hardening

- Every `uses:` is pinned to a full 40-hex commit SHA with a `# vX.Y.Z` comment; Dependabot bumps
  both. `scripts/check-action-pins.sh` (tested with bats by `scripts/check-action-pins.bats`) fails
  on anything else, in CI's `pin-check` job and in the release's `pin-gate`, which fetches every
  workflow at the triggering commit through the API and runs before any other job.
- Both workflows start from `permissions: {}`; each job asks for what it needs. Every job has a
  `timeout-minutes`, and every checkout but the one that pushes the bump uses
  `persist-credentials: false`.
- Rust comes from rustup, at the version pinned in `.mise.toml` (MSRV from `Cargo.toml`); no
  toolchain action. Release jobs restore no cargo cache, and packaging tools are installed at
  pinned versions.
- Release artifacts are built by jobs with `contents: read` only. One `checksums` job hashes and
  attests every asset, and only `publish-assets` can write to the release (with `gh`, not a
  third-party release action).
- Dependabot waits seven days after a release before proposing it (`cooldown`), so a compromised
  version has time to be pulled.
- `mise run check` runs the same pin check, jactionlint and shellcheck locally.

The release flow:

1. `pin-gate` checks the action pins; the `quality` gate runs after it.
2. `release-plz` analyzes commit history on `master`.
3. `release-plz update` edits the next version and `CHANGELOG.md` in the workflow checkout.
4. The workflow commits that release bump directly back to `master` (`[skip ci]`).
5. `release-plz release` creates the tag, publishes to crates.io, and creates the GitHub release.
6. Three read-only build jobs check out the tag:
   - `linux-assets` builds the optimized gnu binaries, replaces `stfg` with the static musl build
     of `stfg`, assembles the tarball, builds `deb` via `cargo-deb` and `rpm` via
     `cargo-generate-rpm`, and verifies both packages contain the binaries, docs, notices, and
     the user unit.
   - `linux-musl-assets` builds the static `x86_64-unknown-linux-musl` tarball with `musl-tools`,
     checks the binaries are static, and smoke-tests them.
   - `macos-assets` builds both macOS targets, signs them (see [Signing](#signing)), and
     smoke-tests the native one.
7. `checksums` writes one `.sha256` sidecar per artifact plus `SHA256SUMS`, and attests the build
   provenance of every artifact.
8. `publish-assets` attaches the artifacts and checksums to the GitHub release and appends the
   macOS signing note to the release notes.
9. `jdx/packslip` then signs a `packslip.sigstore.json` manifest covering the gnu, musl, and macOS
   tarballs (used by `packslip install` and the `mise` packslip backend), recording the commit
   the tag points to. The signing job has read-only access; a separate job uploads the bundle,
   and a third verifies it against the published tarballs and the signer fingerprint in
   `README.md`.
10. It builds and deploys RPM/APT repository metadata to GitHub Pages.

The release workflow uses the default `GITHUB_TOKEN` for repository operations. It does not depend
on a PAT to trigger a second workflow.

## Signing

The macOS `stfgd` and `stfg` are signed with star-forge's own self-signed code-signing
certificate. There is no Apple Developer ID: the project is open source and non-profit, so
releases are not notarized and a browser download still needs
`xattr -dr com.apple.quarantine <dir>`. What the certificate buys is a stable designated
requirement:

```text
designated => identifier "ua.SkeLLLa.star-forge.stfgd" and certificate leaf = H"<sha1>"
```

(`stfg`'s is the same with `ua.SkeLLLa.star-forge.stfg`). macOS keys TCC grants (Full Disk Access,
access to protected folders for `exec` badges) and keychain "Always Allow" entries to this
requirement, so they survive updates. An ad-hoc signature pins the cdhash instead, and every
build looks like a new program.

The certificate is star-forge's alone. cAInary, quartz-drop and local dev builds each use their
own, and the requirement pins the exact leaf, so a key leaked from one can't sign as another.

**Pin.** `packaging/signing/release.sha1` holds the certificate's SHA-1. It is the `codesign`
identity and the pin `packaging/macos/sign.sh` checks: with `CODESIGN_IDENTITY` set, signing fails
unless both the signature and the designated requirement carry that leaf. Once the pin is
committed, the release fails rather than ship ad hoc or with another key; changing the pin makes
every user grant the permissions again, so do it only when the certificate expires (it is valid
for 20 years) or leaks. Until the pin is committed, `macos-assets` signs ad hoc, emits a CI
warning and says so in the release notes.

**Secrets.** The key lives only in the `macos-signing` GitHub environment, used by the
`macos-assets` job of `release.yml`:

| Secret                       | Value                                              |
| ---------------------------- | -------------------------------------------------- |
| `MACOS_CERTIFICATE_P12`      | the .p12 (key and certificate), base64 on one line |
| `MACOS_CERTIFICATE_PASSWORD` | its password                                       |

Set up the environment in **Settings → Environments → New environment** `macos-signing`:

- **Deployment branches and tags:** *Selected branches and tags*, with only `master`. A
  workflow on any other branch or tag that names the environment is refused before it runs.
  Repository secrets have no such rule, so don't keep these as repository secrets.
- **Required reviewers** (optional): yourself, if every release should wait for approval.
- No environment variables are needed; the identity comes from the committed pin.

`ci.yml` never names the environment, and pull requests (including from forks) sign ad hoc. The
binaries are compiled before the key is imported, so no dependency build script or proc macro
runs while it is in a keychain. The import step puts the .p12 into a temporary keychain that
only `/usr/bin/codesign` may use, checks that it holds the pinned identity, and removes it right
after signing. The job holds `contents: read` only.

**Making the certificate.** On the maintainer's Mac (not in CI), with the `macos-signing-cert`
script from the maintainer's dotfiles, or the same steps by hand: an RSA 3072 self-signed
certificate named `ua.SkeLLLa.star-forge`, organization `github.com/SkeLLLa/star-forge`, with
critical `keyUsage = digitalSignature` and `extendedKeyUsage = codeSigning`, exported with
`/usr/bin/openssl` (LibreSSL; the PKCS#12 that OpenSSL 3 writes by default doesn't import).

```sh
macos-signing-cert -g SkeLLLa/star-forge ua.SkeLLLa.star-forge
cp ~/.local/share/macos-signing/ua.SkeLLLa.star-forge.release/ua.SkeLLLa.star-forge.release.sha1 \
  packaging/signing/release.sha1
```

`-g` sets both secrets in the `macos-signing` environment through `gh`. Then back up the .p12
and its password outside the machine and delete the directory.

**Local builds.** Make a separate dev certificate and import it into the login keychain, so dev
builds keep their grants across rebuilds without ever matching the release requirement:

```sh
macos-signing-cert -d -i ua.SkeLLLa.star-forge
cargo build --release
CODESIGN_IDENTITY="$(cat ~/.local/share/macos-signing/ua.SkeLLLa.star-forge.dev/*.sha1)" \
  packaging/macos/sign.sh target/release
codesign -d -r- target/release/stfgd
```

## Crates.io Trusted Publishing

`release.yml` has `id-token: write` on the release job and uses
`rust-lang/crates-io-auth-action` to request a short-lived crates.io token through GitHub Actions
OIDC. That token is passed to release-plz through `CARGO_REGISTRY_TOKEN`; no long-lived Cargo
registry secret is stored in the repository.

The repository is a single `star-forge` crate (the daemon shuts itself down when a client
reports a different version). The `daemon` feature (default) gates the daemon's dependencies, so
`--no-default-features --bin stfg` builds a client that compiles only `libc`.

Trusted publishing cannot create a brand-new crate. Publish the first version manually, then
configure trusted publishing on crates.io for subsequent releases:

1. Publish the initial crate version from a local machine with a normal crates.io token:

   ```bash
   cargo publish --locked
   ```

2. Open the crate's settings on crates.io.
3. Add a trusted publisher with:
   - Publisher: `GitHub`
   - Repository owner: `SkeLLLa`
   - Repository name: `star-forge`
   - Workflow filename: `release.yml`
   - Environment name: empty, unless the workflow later adds a GitHub Actions environment

## First-Run Flow

### `cargo install`

```bash
cargo install --locked star-forge
```

Optional user unit for a Cargo install:

```bash
mkdir -p ~/.config/systemd/user
cat > ~/.config/systemd/user/star-forge.service <<'EOF'
[Unit]
Description=star-forge statusline badge cache daemon
Documentation=https://github.com/SkeLLLa/star-forge

[Service]
Type=simple
ExecStart=%h/.cargo/bin/stfgd daemon
ExecReload=%h/.cargo/bin/stfgd reload
Restart=on-failure
RestartSec=1s

[Install]
WantedBy=default.target
EOF
systemctl --user daemon-reload
systemctl --user enable --now star-forge.service
```

### Binary archive

```bash
tar -xzf star-forge-<version>-x86_64-unknown-linux-gnu.tar.gz
install -m 755 star-forge-<version>-x86_64-unknown-linux-gnu/bin/* ~/.local/bin/
```

To verify a downloaded artifact before installing it, download the matching `.sha256` sidecar and
run:

```bash
sha256sum -c star-forge-<version>-x86_64-unknown-linux-gnu.tar.gz.sha256
```

The release also includes `SHA256SUMS`, merged from every sidecar (Linux, musl, and macOS) once all
assets are attached. To check everything you downloaded into one directory:

```bash
sha256sum -c --ignore-missing SHA256SUMS
```

### Native package

```bash
sudo dpkg -i star-forge_<version>_amd64.deb
```

```bash
sudo rpm -i star-forge-<version>-1.x86_64.rpm
```

Then, optionally, start the daemon with the session:

```bash
systemctl --user daemon-reload
systemctl --user enable --now star-forge.service
```

### RPM repository

```bash
sudo curl -fsSLo /etc/yum.repos.d/star-forge.repo https://skellla.github.io/star-forge/rpm/star-forge.repo
sudo dnf install star-forge
```

### APT repository

```bash
echo 'deb [trusted=yes] https://skellla.github.io/star-forge/deb stable main' | sudo tee /etc/apt/sources.list.d/star-forge.list
sudo apt update
sudo apt install star-forge
```

### Nix

```bash
nix profile install github:SkeLLLa/star-forge
```

The flake builds from source (`packaging/nix/package.nix`) and, on Linux, installs the systemd
user unit to `lib/systemd/user/` with `ExecStart` pointing at the store path.

### mise

```bash
mise use -g github:SkeLLLa/star-forge
```

This installs `stfgd` and `stfg` from the release tarball. No setup is required; the daemon is
spawned on the first `get`, as with `cargo install`. To start it with the session, use the
`cargo install` unit above, with `ExecStart` pointing at the mise-installed `stfgd` path.

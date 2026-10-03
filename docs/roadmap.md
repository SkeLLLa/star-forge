# Roadmap

Planned features, mostly aimed at replacing starship's language-version modules (`nodejs`,
`python`, `rust`, `golang`, ...). Those spawn `<tool> --version` on every prompt, which is slow
behind version-manager shims (nvm, asdf, mise, pyenv), and starship doesn't cache it across
prompts. Cheap, file-reading modules (`aws`, `gcloud`, `package`, `direnv`, `nix_shell`, ...)
gain nothing from a `stfg` call and are out of scope.

## TODO

- [x] **`project_root` scope.** Cache key is the nearest ancestor of `--cwd` containing one of
  `markers = [".nvmrc", "package.json", ".tool-versions"]`; the command runs there. Fixes
  monorepos (`sub/.nvmrc`) and non-git directories, where `git_root` / `global` give a wrong
  version after `cd`.
- [x] **`watch` invalidation.** `watch = [".nvmrc", ".tool-versions", "mise.toml"]`, relative to
  the scope root; the cached value is refreshed when any of them changes (inode, size, ctime,
  same check as config/palette reload). No more waiting for `interval` after editing a
  toolchain file.
- [x] **`when_file` gating.** Return empty without spawning anything when none of the listed
  files exists in the scope root. Lets one badge group hold every language badge.
- [x] **Client env forwarding.** `env = ["PATH", "NVM_BIN", "MISE_*"]` on a badge: `stfg` sends
  those variables, the daemon runs the command with them and makes them part of the cache key,
  so `nvm use` / `mise use` take effect immediately. Today commands run with the daemon's env.
- [x] **`tool_version` builtin.** `tool = "node"`: read the version file first (`.nvmrc`,
  `.node-version`, `.tool-versions`, `rust-toolchain.toml`, ...), fall back to `<tool>
  --version` with a sensible `extract`. Sugar over the four items above.

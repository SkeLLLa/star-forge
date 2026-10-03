{ lib, rustPlatform, stdenv }:

rustPlatform.buildRustPackage {
  pname = "star-forge";
  version = (lib.importTOML ../../Cargo.toml).package.version;

  # `target` may be a symlink to an external cache (mbx); keep it out of non-flake builds.
  src = lib.cleanSourceWith {
    src = lib.cleanSource ../..;
    filter = path: _: baseNameOf path != "target";
  };
  cargoLock.lockFile = ../../Cargo.lock;

  # Integration tests spawn a daemon and use sockets/$HOME; not reliable in the Nix sandbox.
  doCheck = false;

  postInstall = lib.optionalString stdenv.hostPlatform.isLinux ''
    install -Dm644 packaging/systemd/star-forge.service \
      $out/lib/systemd/user/star-forge.service
    substituteInPlace $out/lib/systemd/user/star-forge.service \
      --replace-fail /usr/bin/stfgd $out/bin/stfgd
  '';

  meta = {
    description = "Statusline badge cache daemon and CLI";
    homepage = "https://github.com/SkeLLLa/star-forge";
    license = lib.licenses.gpl3Plus;
    mainProgram = "stfgd";
    platforms = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
  };
}

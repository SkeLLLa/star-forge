{
  description = "star-forge: statusline badge cache daemon";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAll = f: nixpkgs.lib.genAttrs systems (s: f nixpkgs.legacyPackages.${s});
    in
    {
      packages = forAll (pkgs: rec {
        star-forge = pkgs.callPackage ./packaging/nix/package.nix { };
        default = star-forge;
      });
      overlays.default = final: _: { star-forge = final.callPackage ./packaging/nix/package.nix { }; };
    };
}

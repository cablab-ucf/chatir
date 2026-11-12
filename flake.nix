{
  description = "chatir: private, peer-to-peer community chat over iroh";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  outputs = { self, nixpkgs }:
    let
      each = f: nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" ]
        (s: f nixpkgs.legacyPackages.${s});
    in {
      packages = each (pkgs: rec {
        chatir = pkgs.callPackage ./package.nix { };
        default = chatir;
      });
      nixosModules.default = ./nixos.nix;
    };
}

{
  description = "Agens — coding agent CLI";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { nixpkgs, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
      in
      {
        devShells.default = pkgs.mkShell {
          packages = [
            pkgs.go
            pkgs.gopls
            pkgs.gotools
            pkgs.golangci-lint
            pkgs.just
            pkgs.sqlc
          ];

          shellHook = ''
            echo "Agens dev shell (Go, just, golangci-lint)"
          '';
        };

        formatter = pkgs.nixpkgs-fmt;
      });
}

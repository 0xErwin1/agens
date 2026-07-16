{
  description = "Agens — coding agent CLI";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts = {
      url = "git+https://github.com/hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };
    devenv.url = "git+https://github.com/cachix/devenv";
    nix2container = {
      url = "git+https://github.com/nlewo/nix2container";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    mk-shell-bin.url = "git+https://github.com/rrbutani/nix-mk-shell-bin";
    rust-overlay = {
      url = "git+https://github.com/oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-darwin"
        "x86_64-linux"
      ];

      imports = [ inputs.devenv.flakeModule ];

      perSystem = { pkgs, ... }: {
        devenv.shells.default = {
          packages = [
            pkgs.go
            pkgs.gopls
            pkgs.gotools
            pkgs.golangci-lint
            pkgs.jq
            pkgs.just
            pkgs.sqlc
          ];

          languages.rust = {
            enable = true;
            toolchainFile = ./rust-toolchain.toml;
          };

          enterShell = ''
            echo "Agens dev shell (Go, Rust, just, golangci-lint)"
          '';
        };

        formatter = pkgs.nixpkgs-fmt;
      };
    };
}

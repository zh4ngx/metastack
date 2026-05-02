{
  description = "metastack";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-parts,
      rust-overlay,
      ...
    }@inputs:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];

      perSystem =
        { self', system, ... }:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ (import rust-overlay) ];
          };

          rustToolchain = pkgs.rust-bin.stable.latest.default.override {
            extensions = [
              "rust-src"
              "rust-analyzer"
            ];
          };

          rustPlatform = pkgs.makeRustPlatform {
            cargo = rustToolchain;
            rustc = rustToolchain;
          };
        in
        {
          packages.default = rustPlatform.buildRustPackage {
            pname = "metastack";
            version = "0.5.0";
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;
          };

          apps.default = {
            type = "app";
            program = "${self'.packages.default}/bin/metastack";
          };

          devShells.default = pkgs.mkShell {
            packages = with pkgs; [
              rustToolchain
              cargo-nextest
              cargo-watch
              rustfmt
              clippy
            ];

            shellHook = ''
              echo "metastack dev shell"
              echo "Rust: $(rustc --version)"
              echo ""
              echo "Commands:"
              echo "  cargo check"
              echo "  cargo test"
              echo "  cargo watch -x test"
            '';
          };
        };
    };
}

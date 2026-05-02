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

          cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
          cargoLock = builtins.fromTOML (builtins.readFile ./Cargo.lock);
          matchingLockPackages = builtins.filter (package: package.name == "metastack") cargoLock.package;
          metastackLockPackage =
            if matchingLockPackages == [ ] then
              throw "Cargo.lock is missing the metastack package"
            else
              builtins.head matchingLockPackages;
        in
        {
          packages.default = rustPlatform.buildRustPackage {
            pname = "metastack";
            version = cargoToml.package.version;
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;
          };

          apps.default = {
            type = "app";
            program = "${self'.packages.default}/bin/metastack";
            meta.description = "Run the metastack CLI";
          };

          checks.default = self'.packages.default;

          checks.version = pkgs.runCommand "metastack-version-check" { } ''
            cargo_version='${cargoToml.package.version}'
            lock_version='${metastackLockPackage.version}'
            package_version='${self'.packages.default.version}'
            if [ "$cargo_version" != "$lock_version" ]; then
              echo "Cargo.toml version $cargo_version does not match Cargo.lock version $lock_version" >&2
              exit 1
            fi
            if [ "$cargo_version" != "$package_version" ]; then
              echo "Cargo.toml version $cargo_version does not match Nix package version $package_version" >&2
              exit 1
            fi
            touch "$out"
          '';

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

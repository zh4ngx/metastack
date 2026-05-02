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

          rustBuildToolchain = pkgs.rust-bin.stable.latest.minimal;

          rustDevToolchain = pkgs.rust-bin.stable.latest.default.override {
            extensions = [
              "rust-src"
              "rust-analyzer"
            ];
          };

          rustPlatform = pkgs.makeRustPlatform {
            cargo = rustBuildToolchain;
            rustc = rustBuildToolchain;
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
            meta = with pkgs.lib; {
              description = cargoToml.package.description;
              homepage = cargoToml.package.homepage;
              license = licenses.mit;
              mainProgram = "metastack";
              platforms = platforms.linux ++ platforms.darwin;
            };
          };

          apps.default = {
            type = "app";
            program = "${self'.packages.default}/bin/metastack";
            meta.description = "Run the metastack CLI";
          };

          checks.default = self'.packages.default;

          checks.smoke = pkgs.runCommand "metastack-smoke-check" { } ''
            version_output="$(${self'.packages.default}/bin/metastack --version)"
            if [ "$version_output" != "metastack ${cargoToml.package.version}" ]; then
              echo "unexpected --version output: $version_output" >&2
              exit 1
            fi
            ${self'.packages.default}/bin/metastack --help | grep -q "Structured send"
            touch "$out"
          '';

          checks.no-rust-toolchain-reference =
            pkgs.runCommand "metastack-no-rust-toolchain-reference" { } ''
              if grep -R -F '${rustBuildToolchain}' ${self'.packages.default} >/dev/null; then
                echo "metastack output references build Rust toolchain" >&2
                exit 1
              fi
              if grep -R -F '${rustDevToolchain}' ${self'.packages.default} >/dev/null; then
                echo "metastack output references dev Rust toolchain" >&2
                exit 1
              fi
              touch "$out"
            '';

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
            if ! grep -q "github:zh4ngx/metastack/v$cargo_version" ${./README.md}; then
              echo "README.md install examples do not reference v$cargo_version" >&2
              exit 1
            fi
            if ! grep -q "## v$cargo_version " ${./CHANGELOG.md}; then
              echo "CHANGELOG.md is missing an entry for v$cargo_version" >&2
              exit 1
            fi
            touch "$out"
          '';

          devShells.default = pkgs.mkShell {
            packages = with pkgs; [
              rustDevToolchain
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

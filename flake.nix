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
      flake = {
        nixosModules.default = import ./nix/modules/nixos.nix { inherit self; };
        homeModules.default = import ./nix/modules/home-manager.nix { inherit self; };
      };

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

          rustMsrvVersion =
            let
              version = cargoToml.package."rust-version";
            in
            if builtins.match "[0-9]+\\.[0-9]+" version != null then
              "${version}.0"
            else
              version;
          rustBuildToolchain = pkgs.rust-bin.stable.latest.minimal;
          rustMsrvToolchain = pkgs.rust-bin.stable.${rustMsrvVersion}.minimal;

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

          msrvRustPlatform = pkgs.makeRustPlatform {
            cargo = rustMsrvToolchain;
            rustc = rustMsrvToolchain;
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

          checks.msrv = msrvRustPlatform.buildRustPackage {
            pname = "metastack-msrv";
            version = cargoToml.package.version;
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;
          };

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

          checks.nixos-module =
            let
              overridePackage = pkgs.runCommand "metastack-nixos-module-override" { } ''
                touch "$out"
              '';
              evalEnabled = pkgs.lib.evalModules {
                specialArgs = { inherit pkgs; };
                modules = [
                  self.nixosModules.default
                  (
                    { lib, ... }:
                    {
                      options.environment.systemPackages = lib.mkOption {
                        type = lib.types.listOf lib.types.package;
                        default = [ ];
                      };
                      config.programs.metastack = {
                        enable = true;
                        package = overridePackage;
                      };
                    }
                  )
                ];
              };
              evalDisabled = pkgs.lib.evalModules {
                specialArgs = { inherit pkgs; };
                modules = [
                  self.nixosModules.default
                  (
                    { lib, ... }:
                    {
                      options.environment.systemPackages = lib.mkOption {
                        type = lib.types.listOf lib.types.package;
                        default = [ ];
                      };
                    }
                  )
                ];
              };
              enabledPackages = evalEnabled.config.environment.systemPackages;
              disabledPackages = evalDisabled.config.environment.systemPackages;
              overrideMatches = builtins.head enabledPackages == overridePackage;
            in
            pkgs.runCommand "metastack-nixos-module-check" { } ''
              package_count='${toString (builtins.length enabledPackages)}'
              if [ "$package_count" != 1 ]; then
                echo "expected NixOS module to install one package, got $package_count" >&2
                exit 1
              fi
              if [ '${if overrideMatches then "yes" else "no"}' != yes ]; then
                echo "expected NixOS module package override to be installed" >&2
                exit 1
              fi
              disabled_package_count='${toString (builtins.length disabledPackages)}'
              if [ "$disabled_package_count" != 0 ]; then
                echo "expected disabled NixOS module to install no packages, got $disabled_package_count" >&2
                exit 1
              fi
              touch "$out"
            '';

          checks.home-manager-module =
            let
              overridePackage = pkgs.runCommand "metastack-home-manager-module-override" { } ''
                touch "$out"
              '';
              evalEnabled = pkgs.lib.evalModules {
                specialArgs = { inherit pkgs; };
                modules = [
                  self.homeModules.default
                  (
                    { lib, ... }:
                    {
                      options.home.packages = lib.mkOption {
                        type = lib.types.listOf lib.types.package;
                        default = [ ];
                      };
                      options.xdg.configFile = lib.mkOption {
                        type = lib.types.attrsOf lib.types.anything;
                        default = { };
                      };
                      config.programs.metastack = {
                        enable = true;
                        package = overridePackage;
                        routingConfig = {
                          version = 2;
                          backends.codex = {
                            type = "codex";
                            url = "ws://127.0.0.1:4107";
                          };
                          agents.local-codex = {
                            backend = "codex";
                            cwd = "/tmp/project";
                          };
                        };
                      };
                    }
                  )
                ];
              };
              evalDisabled = pkgs.lib.evalModules {
                specialArgs = { inherit pkgs; };
                modules = [
                  self.homeModules.default
                  (
                    { lib, ... }:
                    {
                      options.home.packages = lib.mkOption {
                        type = lib.types.listOf lib.types.package;
                        default = [ ];
                      };
                      options.xdg.configFile = lib.mkOption {
                        type = lib.types.attrsOf lib.types.anything;
                        default = { };
                      };
                    }
                  )
                ];
              };
              enabledPackages = evalEnabled.config.home.packages;
              disabledPackages = evalDisabled.config.home.packages;
              disabledConfigFiles = evalDisabled.config.xdg.configFile;
              routingSource = evalEnabled.config.xdg.configFile."metastack/routing.yaml".source;
              overrideMatches = builtins.head enabledPackages == overridePackage;
            in
            pkgs.runCommand "metastack-home-manager-module-check" { } ''
              package_count='${toString (builtins.length enabledPackages)}'
              if [ "$package_count" != 1 ]; then
                echo "expected Home Manager module to install one package, got $package_count" >&2
                exit 1
              fi
              if [ '${if overrideMatches then "yes" else "no"}' != yes ]; then
                echo "expected Home Manager module package override to be installed" >&2
                exit 1
              fi
              if ! grep -q "local-codex" ${routingSource}; then
                echo "expected Home Manager module to render routing config" >&2
                exit 1
              fi
              disabled_package_count='${toString (builtins.length disabledPackages)}'
              if [ "$disabled_package_count" != 0 ]; then
                echo "expected disabled Home Manager module to install no packages, got $disabled_package_count" >&2
                exit 1
              fi
              disabled_config_count='${toString (builtins.length (builtins.attrNames disabledConfigFiles))}'
              if [ "$disabled_config_count" != 0 ]; then
                echo "expected disabled Home Manager module to render no config files, got $disabled_config_count" >&2
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

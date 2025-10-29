{
  description = "nunu.ai CLI for uploading build artifacts";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    crane.url = "github:ipetkov/crane";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    flake-utils.url = "github:numtide/flake-utils";

    advisory-db = {
      url = "github:rustsec/advisory-db";
      flake = false;
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      fenix,
      flake-utils,
      advisory-db,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};

        toolchain =
          with fenix.packages.${system};
          combine (
            [
              default.toolchain
              stable.rust-src
            ]
            ++ lib.optionals pkgs.stdenv.isLinux [
              targets.x86_64-pc-windows-gnu.latest.rust-std
              targets.x86_64-unknown-linux-musl.latest.rust-std
            ]
          );

        inherit (pkgs) lib;

        craneLib = (crane.mkLib pkgs).overrideToolchain toolchain;
        src = craneLib.cleanCargoSource ./.;

        # Common arguments can be set here to avoid repeating them later
        commonArgs = {
          inherit src;
          strictDeps = true;
          doCheck = false;

          buildInputs =
            [ ]
            ++ lib.optionals pkgs.stdenv.isDarwin [
              pkgs.libiconv
            ];
        };

        # Build *just* the cargo dependencies, so we can reuse
        # all of that work (e.g. via cachix) when running in CI
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        # Regular build (for local development and Darwin)
        nunu-cli = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts;
          }
        );

        # Static Linux build (Linux only - for releases)
        nunu-cli-linux-musl = lib.optionalAttrs pkgs.stdenv.isLinux (
          craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              CARGO_BUILD_TARGET = "x86_64-unknown-linux-musl";
              CARGO_BUILD_RUSTFLAGS = "-C target-feature=+crt-static";
            }
          )
        );

        # Windows build (Linux only - cross-compilation)
        nunu-cli-windows = lib.optionalAttrs pkgs.stdenv.isLinux (
          craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              CARGO_BUILD_TARGET = "x86_64-pc-windows-gnu";

              # fixes issues related to libring
              TARGET_CC = "${pkgs.pkgsCross.mingwW64.stdenv.cc}/bin/${pkgs.pkgsCross.mingwW64.stdenv.cc.targetPrefix}cc";

              # fixes issues related to openssl
              OPENSSL_DIR = "${pkgs.openssl.dev}";
              OPENSSL_LIB_DIR = "${pkgs.openssl.out}/lib";
              OPENSSL_INCLUDE_DIR = "${pkgs.openssl.dev}/include/";

              depsBuildBuild = with pkgs; [
                pkgsCross.mingwW64.stdenv.cc
                pkgsCross.mingwW64.windows.pthreads
              ];
            }
          )
        );

        scripts = {
          bump-version = pkgs.writeShellScriptBin "bump-version" ''
            set -euo pipefail
            export PATH="$PATH:${
              pkgs.lib.makeBinPath [
                pkgs.convco
                pkgs.cargo
                pkgs.cargo-release
              ]
            }"

            cur_version=$(convco version --prefix v)
            version=$(convco version --bump --prefix v)
            if [ "$cur_version" = "$version" ]; then
              echo "nunu-cli does not require version bump from v$version"
              exit 0
            fi
            echo "Releasing nunu-cli v$version"
            cargo release --no-confirm --no-publish --execute $version
          '';
        };
      in
      {
        checks = {
          # Run clippy (and deny all warnings) on the crate source
          nunu-cli-clippy = craneLib.cargoClippy (
            commonArgs
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--all-targets -- --deny warnings";
            }
          );

          nunu-cli-doc = craneLib.cargoDoc (
            commonArgs
            // {
              inherit cargoArtifacts;
            }
          );

          # Check formatting
          nunu-cli-fmt = craneLib.cargoFmt {
            inherit src;
          };

          nunu-cli-toml-fmt = craneLib.taploFmt {
            src = pkgs.lib.sources.sourceFilesBySuffices src [ ".toml" ];
          };

          # Audit dependencies
          nunu-cli-audit = craneLib.cargoAudit {
            inherit src advisory-db;
          };

          # Audit licenses
          nunu-cli-deny = craneLib.cargoDeny {
            inherit src;
          };

          # Run tests with cargo-nextest
          nunu-cli-nextest = craneLib.cargoNextest (
            commonArgs
            // {
              inherit cargoArtifacts;
              partitions = 1;
              partitionType = "count";
            }
          );
        };

        packages =
          {
            default = nunu-cli;

            # Platform-specific release artifacts
            release-artifacts =
              if pkgs.stdenv.isLinux then
                # On Linux: build static Linux binary + Windows binary
                pkgs.stdenv.mkDerivation {
                  name = "nunu-cli-release";

                  buildInputs = [
                    nunu-cli-linux-musl
                    nunu-cli-windows
                  ];

                  unpackPhase = "true";

                  installPhase = ''
                    mkdir -p $out/bin

                    # Copy static Linux binary
                    cp ${nunu-cli-linux-musl}/bin/nunu-cli $out/bin/nunu-cli

                    # Copy Windows binary
                    cp ${nunu-cli-windows}/bin/nunu-cli.exe $out/bin/nunu-cli.exe
                  '';
                }
              else
                # On macOS: just build native macOS binary
                pkgs.stdenv.mkDerivation {
                  name = "nunu-cli-release";

                  buildInputs = [ nunu-cli ];

                  unpackPhase = "true";

                  installPhase = ''
                    mkdir -p $out/bin

                    # Copy native binary
                    cp ${nunu-cli}/bin/nunu-cli $out/bin/nunu-cli
                  '';
                };
          }
          // scripts
          // lib.optionalAttrs pkgs.stdenv.isLinux {
            # These packages only available on Linux
            linux-musl = nunu-cli-linux-musl;
            windows = nunu-cli-windows;
          };

        apps.default = flake-utils.lib.mkApp {
          drv = nunu-cli;
        };

        devShells.default = craneLib.devShell {
          # Inherit inputs from checks.
          checks = self.checks.${system};

          # Extra inputs can be added here; cargo and rustc are provided by default.
          packages = with pkgs; [
            just
            pre-commit
            cargo-release
            convco
          ];

          shellHook = ''
            pre-commit install > /dev/null
          '';
        };
      }
    );
}

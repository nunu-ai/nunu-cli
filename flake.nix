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
          combine [
            default.toolchain
            stable.rust-src
            targets.x86_64-pc-windows-gnu.latest.rust-std
          ];

        inherit (pkgs) lib;

        craneLib = (crane.mkLib pkgs).overrideToolchain toolchain;
        src = craneLib.cleanCargoSource ./.;

        # Common arguments can be set here to avoid repeating them later
        commonArgs = {
          inherit src;
          strictDeps = true;
          doCheck = false;

          buildInputs = [
            # Add additional build inputs here
          ]
          ++ lib.optionals pkgs.stdenv.isDarwin [
            pkgs.libiconv
          ];
        };

        # Build *just* the cargo dependencies, so we can reuse
        # all of that work (e.g. via cachix) when running in CI
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        # Build the actual crate itself, reusing the dependency
        # artifacts from above.
        nunu-cli = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts;
          }
        );

        nunu-cli-windows = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts;
            CARGO_BUILD_TARGET = "x86_64-pc-windows-gnu";

            # fixes issues related to libring
            TARGET_CC = "${pkgs.pkgsCross.mingwW64.stdenv.cc}/bin/${pkgs.pkgsCross.mingwW64.stdenv.cc.targetPrefix}cc";

            #fixes issues related to openssl
            OPENSSL_DIR = "${pkgs.openssl.dev}";
            OPENSSL_LIB_DIR = "${pkgs.openssl.out}/lib";
            OPENSSL_INCLUDE_DIR = "${pkgs.openssl.dev}/include/";

            depsBuildBuild = with pkgs; [
              pkgsCross.mingwW64.stdenv.cc
              pkgsCross.mingwW64.windows.pthreads
            ];
          }
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

            cur_version=$(convco version --prefix nunu-cli-v)
            version=$(convco version --bump --prefix nunu-cli-v)
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
          # Build the crate as part of `nix flake check` for convenience

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

        packages = {
          default = nunu-cli;
          windows = nunu-cli-windows;

          release-artifacts = pkgs.symlinkJoin {
            name = "nunu-cli-release";
            paths = [
              nunu-cli
              nunu-cli-windows
            ];
          };
        }
        // scripts;

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

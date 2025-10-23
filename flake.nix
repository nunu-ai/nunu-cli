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
            stable.toolchain
            stable.rust-src
          ];

        inherit (pkgs) lib;

        craneLib = (crane.mkLib pkgs).overrideToolchain toolchain;
        src = craneLib.cleanCargoSource ./.;

        # Common arguments can be set here to avoid repeating them later
        commonArgs = {
          inherit src;
          strictDeps = true;
          doCheck = false;

          nativeBuildInputs = [
            pkgs.pkg-config
          ];

          buildInputs =
            [
              # Add additional build inputs here
            ]
            ++ lib.optionals pkgs.stdenv.isDarwin [
              pkgs.libiconv
            ];

          # Override rustc flags to use default linker instead of lld
          # This avoids posix_spawnp issues in Nix sandbox with fenix toolchain
          RUSTFLAGS = "-C link-arg=-fuse-ld=gold";

          # Additional environment variables can be set directly
          # MY_CUSTOM_VAR = "some value";
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
      in
      {
        checks = {
          # Build the crate as part of `nix flake check` for convenience
          inherit nunu-cli;

          # Run clippy (and deny all warnings) on the crate source,
          # again, reusing the dependency artifacts from above.
          #
          # Note that this is done as a separate derivation so that
          # we can block the CI if there are issues here, but not
          # prevent downstream consumers from building our crate by itself.
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
            # taplo arguments can be further customized below as needed
            # taploExtraArgs = "--config ./taplo.toml";
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
          # Consider setting `doCheck = false` on `my-crate` if you do not want
          # the tests to run twice
          nunu-cli-nextest = craneLib.cargoNextest (
            commonArgs
            // {
              inherit cargoArtifacts;
              partitions = 1;
              partitionType = "count";
              cargoNextestPartitionsExtraArgs = "--no-tests=pass";
            }
          );
        };

        packages = {
          default = nunu-cli;
          windows = nunu-cli-windows;
        };

        apps.default = flake-utils.lib.mkApp {
          drv = nunu-cli;
        };

        devShells.default = craneLib.devShell {
          # Inherit inputs from checks.
          checks = self.checks.${system};

          # Additional dev-shell environment variables can be set directly
          # MY_CUSTOM_DEVELOPMENT_VAR = "something else";

          # Extra inputs can be added here; cargo and rustc are provided by default.
          packages = [
            pkgs.just
            pkgs.pre-commit
          ];

          shellHook = ''
            pre-commit install > /dev/null
          '';
        };
      }
    );
}

default:
  just -l

fix:
  taplo fmt *.toml crates/**/*.toml
  cargo fmt
  cargo clippy --fix --allow-dirty

check:
  nix flake check

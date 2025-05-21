# Nunu Rust Template

Rust project template with error handling, logging, and monitoring pre-configured.

## Features

- **Structured Error Handling**: Uses `thiserror` for defining custom error types and `anyhow` for flexible error propagation
- **Logging Integration**: Pre-configured `env_logger` with sensible defaults
- **Sentry Integration**: Error monitoring and performance tracking setup
- **Nix Integration**: Development environment, packaging and CI with `nix` and `direnv`
- **Code Quality Tools**: Pre-commit hooks for consistent code formatting and quality
- **Modular Architecture**: Clear separation between library and binary code

## Setup

### Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) (latest stable version)
- [Nix](https://nixos.org/download.html) (optional, for reproducible development environment)
- [direnv](https://direnv.net/) (optional, for automatic environment loading)
- [uv](https://github.com/astral-sh/uv) (for installing pre-commit)

### Getting Started

```sh
# 1. Clone this repository:
git clone https://github.com/your-org/nunu-rust-template.git
cd nunu-rust-template

# 2. If using Nix and direnv:
# This will automatically set up the development environment as defined in `flake.nix`.
direnv allow

# 3. Install pre-commit hooks:
uvx pre-commit install

# 4. Build the project:
cargo build
# - or -
nix build

nix build .#rust-template-windows # cross-compile for windows

# 5. Run tests:
cargo test
# - or -
nix flake check

### Environment Variables

To enable Sentry integration, set:
```sh
export SENTRY_DSN=https://your-sentry-dsn
```

To adjust logging levels (default is info):
```sh
export RUST_LOG=debug  # Options: trace, debug, info, warn, error
```

## Project Structure

```
rust_template/
├── .cargo/           # Cargo configuration
├── .editorconfig     # Editor configuration for consistent formatting
├── .envrc            # direnv configuration
├── .github/          # GitHub workflows and templates
├── .gitignore        # Git ignore patterns
├── .pre-commit-config.yaml # Pre-commit hooks configuration
├── src/              # Source files
│   ├── bin/          # Binary executables
│   │   └── main.rs   # Main application entry point
│   └── lib.rs        # Library code
├── tests/            # Integration tests
│   └── dummy.rs      # Example test file
├── Cargo.lock        # Locked dependencies
├── Cargo.toml        # Project dependencies and configuration
├── deny.toml         # Dependency auditing configuration
├── flake.lock        # Locked Nix dependencies
├── flake.nix         # Nix development environment
├── taplo.toml        # TOML formatter configuration
└── README.md         # This file
```

## Development Guidelines

- **Library Code**: Write all reusable logic as library code in `src/lib.rs` and additional modules
- **Binary Code**: Create thin wrappers around library functionality in `src/bin/`
- **Unit Tests**: Write unit tests directly in the files of the code you test
- **Integration Tests**: Write end-to-end and integration tests in `tests/` (see `tests/dummy.rs` for an example)
- **Error Handling**: Define custom errors in the library code using `thiserror`
- **Logging**: Use the `log` crate macros (`error!`, `warn!`, `info!`, `debug!`, `trace!`)

# dev

A devcontainer management CLI built in Rust. Create, build, run, and manage [devcontainers](https://containers.dev/) from the terminal — no VS Code required.

## Features

- **Multiple container runtimes** — Docker, Podman, and Apple Containers (macOS native via XPC)
- **Template system** — scaffold `.devcontainer/` configs from the devcontainer template registry or saved global templates
- **Feature support** — browse, install, and manage devcontainer features with OCI registry integration
- **BuildKit support** — optional BuildKit-optimized image builds
- **Layered configuration** — base config, global templates, and per-workspace overrides
- **VS Code integration** — open running containers in VS Code or VS Code Insiders

## Installation

```sh
cargo install devcontainer
```

Or from source:

```sh
cargo install --path .
```

## Usage

```sh
# Scaffold a minimal .devcontainer/ in the current directory
dev init

# Create from a template (interactive picker or by ID)
dev new
dev new --template ghcr.io/devcontainers/templates/rust

# Build the devcontainer image
dev build
dev build --buildkit --no-cache

# Start the container
dev up
dev up --rebuild

# Run a command or open a shell
dev exec -- cargo test
dev shell

# Stop or remove
dev down
dev down --remove

# Open in VS Code
dev open
dev open --insiders

# Check container state
dev status
dev status --json

# Browse templates and features
dev list templates
dev list features -q rust

# Edit devcontainer config properties
dev config set image mcr.microsoft.com/devcontainers/rust:latest
dev config add features ghcr.io/devcontainers/features/node:1
dev config list

# Manage global templates
dev global new --name my-rust
dev global list
dev global edit my-rust
dev global remove my-rust

# Manage base config (applied to all containers)
dev base edit
dev base config set remoteUser vscode
```

### Global flags

| Flag | Description |
|------|-------------|
| `--workspace <path>` | Override workspace directory (default: `.`) |
| `--runtime <runtime>` | Override container runtime (`docker`, `podman`, `apple`) |
| `-v`, `-vv`, `-vvv` | Increase verbosity |

## Building from source

```sh
cargo build --release
```

The binary will be at `target/release/dev`.

**Note:** The `apple-container` crate (Apple Containers runtime) is only compiled on macOS.

## License

See [LICENSE](LICENSE).

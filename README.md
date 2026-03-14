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

## Layered configuration

`dev` uses a layered config system so you can set preferences once and have them apply everywhere. When you run `dev new` or `dev up`, these layers merge together to produce the final `devcontainer.json`.

### Base config

The **base config** (`~/.dev/base/devcontainer.json`) holds user-wide defaults that apply to every container you create — things like your preferred shell features, editor settings, or environment variables.

```sh
# Open the base config in your editor
dev base edit

# Or set individual properties
dev base config set remoteUser vscode
dev base config add features ghcr.io/devcontainers/features/common-utils:2
dev base config add remoteEnv EDITOR=vim
```

If no base config exists, this layer is simply skipped.

### Global templates

**Global templates** (`~/.dev/global/<name>/`) are reusable devcontainer setups you save once and apply to any project. Think of them as personal starter kits — a "rust" template with your preferred Rust toolchain, a "node" template with your Node setup, etc.

```sh
# Create a new global template (interactive — picks a registry template, options, and features)
dev global new --name my-rust

# List saved templates
dev global list

# Edit a template's config
dev global edit my-rust
dev global config my-rust add features ghcr.io/devcontainers/features/node:1

# Remove a template
dev global remove my-rust
```

### How layers merge

When you create a workspace with `dev new` or bring one up with `dev up`, the config layers merge in this order (lowest to highest priority):

```
1. Global template     (~/.dev/global/<name>/...)
2. Base config         (~/.dev/base/devcontainer.json)
3. Runtime config      (~/.dev/<runtime>/devcontainer.json)
4. Per-project config  (.devcontainer/devcontainer.json or recipe)
```

Higher-priority layers override lower ones, with merge behavior depending on the field type:

| Field type | Merge strategy | Examples |
|-----------|----------------|----------|
| Scalar | Higher priority wins | `image`, `remoteUser` |
| Array | Concatenate (deduplicated) | `mounts`, `forwardPorts`, `runArgs` |
| Map | Merge (higher priority keys win) | `remoteEnv`, `containerEnv` |
| Features | Union (all features combined) | `features` |

**Example:** if your global template sets `image: rust:latest` and your base config sets `remoteUser: vscode` with a zsh feature, the final config gets the Rust image, the vscode user, and both sets of features combined.

### Workspace vs user scope

`dev new` lets you choose where the config lives:

- **Workspace scope** — writes to `.devcontainer/devcontainer.json` in the project (committed to git, shared with the team)
- **User scope** — writes a lightweight recipe to `~/.dev/devcontainers/<folder>/` (keeps the workspace clean, personal to you)

User-scoped recipes reference a global template by name and store any per-project overrides. The full config is composed at build/run time.

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

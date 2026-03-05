## `dev` — A Dev Container CLI

`dev` is a Rust CLI tool for managing [dev containers](https://containers.dev). It reads standard `devcontainer.json` configurations and handles the full container lifecycle — creating, starting, executing commands, and tearing down development environments — all from a single `dev` command.

### Key features

- **Multi-runtime support** — Works with Docker, Podman, and Apple Containers. Auto-detects which runtime is available, preferring Apple Containers on macOS. Can be overridden with `--runtime`.
- **Standard devcontainer.json** — Parses and merges devcontainer configs, including features, mounts, ports, environment variables, lifecycle hooks, and variable substitution.
- **Template system** — Scaffold new projects from the official devcontainer template registry (`dev new`), with an interactive TUI picker for browsing templates and features.
- **Configuration layers** — Supports base configs (user-wide defaults), global templates (reusable named configs), and per-workspace configs, merged together at runtime.
- **Native Apple Containers integration** — The `apple-container` crate talks directly to the Apple Container daemon via XPC and gRPC, with no CLI shelling. Handles container lifecycle (create, bootstrap, stop, delete) over XPC and image builds via a bidirectional gRPC stream to the BuildKit builder VM.

### Commands

| Command | Description |
|---------|-------------|
| `dev init` | Create a minimal `.devcontainer/` with a Dockerfile |
| `dev new` | Create `.devcontainer/` from a template (interactive) |
| `dev build` | Build the devcontainer image |
| `dev up` | Start the devcontainer for the current directory |
| `dev down` | Stop (and optionally remove) the container |
| `dev exec` | Run a command inside the container |
| `dev shell` | Open an interactive shell |
| `dev status` | Show container state |
| `dev config` | View/modify devcontainer configuration |
| `dev list` | Browse available templates and features |

### Architecture

```
src/
  cli.rs            — Clap CLI definition
  commands/         — One module per subcommand
  devcontainer/     — Config parsing, merging, variables, features, lifecycle hooks
  runtime/          — ContainerRuntime trait + Docker, Podman, Apple implementations
  oci/              — OCI registry client for fetching templates/features
  collection/       — Template/feature index caching
  tui/              — Interactive picker and prompts

crates/
  apple-container/  — Native XPC/gRPC client for Apple Containers
  buildkit-client-patch/ — BuildKit gRPC client for image builds
```

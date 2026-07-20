# dev

A terminal-first devcontainer CLI. Create, build, and run [devcontainers](https://containers.dev/) from the shell — no VS Code required.

## What it does

`dev` lets you and AI coding agents work inside a devcontainer without needing an editor. You scaffold a project, build the image, start the container, and run commands — all from the terminal. If you want, you can attach VS Code to the running container; if you don't, everything still works.

The config is a `recipe.json` — a small file that names a reusable global template, stores per-project overrides, and gets composed with your base config and runtime at build/run time. Edit `recipe.json` or `~/.dev/base/devcontainer.json` and the next `dev up` picks up the change without regenerating state.

## Getting started

```sh
# Scaffold a minimal .devcontainer/ in the current project
dev init

# Pick a template, choose features, and choose workspace or user scope
dev new

# Build the image (or just start it — `dev up` builds if it hasn't yet)
dev build
dev up

# Run a command or open an interactive shell
dev exec -- cargo test
dev shell

# Stop or remove
dev down
dev down --remove
```

## Layered configuration

`dev` merges four layers at build/run time, lowest priority first:

```
1. Global template     (~/.dev/global/<name>/...)
2. Base config         (~/.dev/base/devcontainer.json)
3. Runtime config      (~/.dev/<runtime>/devcontainer.json)
4. Per-project recipe  (.devcontainer/recipe.json)
```

A project with its own `.devcontainer/devcontainer.json` merges only the base layer beneath it. Whichever of `image`, `build`, or `dockerComposeFile` the project declares is authoritative — competing selectors from the base are dropped, not merged.

### Base config

Your user-wide defaults — preferred shell, editor settings, environment variables. Open it in your editor or set individual properties:

```sh
dev base edit
dev base config set remoteUser vscode
```

Skip it for a single run with `--no-base`:

```sh
dev up --no-base
```

### Global templates

Reusable devcontainer setups you save once and apply anywhere. A "rust" template with your toolchain, a "node" template with your Node setup. Each machine needs a global template of that name in `~/.dev/global/`; on a fresh clone, `dev new` creates it.

```sh
dev global new --name my-rust
dev global list
dev global edit my-rust
dev global remove my-rust
```

### Recipe projects

A recipe names a global template and stores overrides. It has no `devcontainer.json` on disk — `dev up` and `dev build` compose it in memory with the current base and runtime layers. Editing `~/.dev/base/devcontainer.json` takes effect on the next run without regenerating project state.

Because a recipe project has no `devcontainer.json`, VS Code's "Reopen in Container" has nothing to read. Run `dev up` and attach your editor to the running container instead. `dev vscode repair` re-links user-scoped projects that predate recipes.

## Container runtimes

`dev` supports three runtimes, selected by `--runtime` or auto-detected:

- **Docker** — the default on every platform
- **Podman** — drop-in compatible, no daemon
- **Apple Containers** — macOS native via XPC (compiled with `--features apple`)

Docker Compose projects get a full lifecycle path: build, layer features, UID remap, generate a compose override injecting labels/env/mounts/ports, start services, run lifecycle hooks.

## VS Code integration

Optional. Attach your editor to a running container:

```sh
dev open
dev open --insiders
```

User-scoped projects predate recipes and keep a real `devcontainer.json`; `dev vscode repair` re-links them. Recipe projects skip the link — they compose at runtime.

## Local domain routing

Each project gets a `.test` hostname (`appname.test`) via Caddy and dnsmasq. When `forwardPorts` is configured, `dev up` writes a Caddy fragment, reloads, and prints the URL:

```sh
dev up
# Container 'appname' is ready.
#   → https://appname.test → port 3000
```

## Commands reference

| Command | What it does |
|---------|-------------|
| `dev init` | Scaffold minimal `.devcontainer/` with Dockerfile |
| `dev new` | Pick a template, features, and scope; write a recipe |
| `dev build` | Build the image (with optional `--no-cache`, `--frozen-lockfile`, `--no-base`) |
| `dev up` | Start the container (with `--rebuild`, `--ports`, `--no-base`) |
| `dev exec` | Run a command in the running container |
| `dev shell` | Open an interactive shell |
| `dev down` | Stop (optionally `--remove`) |
| `dev list templates` / `dev list features` | Browse the template and feature registries |
| `dev status` | Show container state |
| `dev config set/unset/add/remove/list` | Edit devcontainer properties |
| `dev global new/list/edit/remove` | Manage global templates |
| `dev base new/edit/config` | Manage base config |
| `dev forward` | Forward a local port (with `--daemon`, `--stop`, `--list`) |
| `dev open` / `dev vscode repair` | VS Code integration |

Global flags: `--workspace <path>`, `--runtime <runtime>`, `-v` / `-vv` / `-vvv`.

## Derived images and disk use

When a config declares features, `dev up` and `dev build` layer them onto a derived image tagged `<folder>-features-<digest>`. The digest covers the effective config values — base image selector, features, `remoteUser`, `containerEnv`, `remoteEnv`. Edit any of those and the next run builds a new image rather than reusing a stale one.

The digest covers selectors, not the files they point at. Editing a `Dockerfile` referenced by `build.dockerfile` leaves the digest unchanged; pass `--rebuild` or `--no-cache` after changing Dockerfile contents.

Superseded images accumulate rather than being overwritten in place. Reclaim space with your runtime's own tooling:

```sh
docker image ls --filter 'reference=<folder>-features-*'
docker image prune
```

## Installation

```sh
cargo install devcontainer
```

Or from source:

```sh
cargo install --path .
```

**Note:** The Apple Containers runtime is opt-in — it is only available on macOS and only when built with `--features apple`:

```sh
cargo install --path . --features apple
```

That build needs `protoc` on `PATH`; see [CONTRIBUTING.md](CONTRIBUTING.md#development) for the prerequisites.

## Safety

`dev` manages containers and interacts with container runtimes and OCI registries. Security concerns include container escape, unsafe config handling, credential exposure, and host filesystem operations. See [SECURITY.md](SECURITY.md) for reporting a vulnerability.

## Contributing

Fork, clone, branch, and submit a PR against `main`. Install the Rust toolchain (stable), run `cargo test` and `cargo clippy` before submitting, and keep commits focused. See [CONTRIBUTING.md](CONTRIBUTING.md) for full details.

## License

MIT — see [LICENSE](LICENSE).
# dev

A devcontainer management CLI built in Rust. Create, build, run, and manage [devcontainers](https://containers.dev/) from the terminal — no VS Code required.

## Features

- **Multiple container runtimes** — Docker, Podman, and Apple Containers (macOS native via XPC)
- **Docker Compose support** — full lifecycle for `dockerComposeFile`-based configs (build, up, down, shell, features, UID remapping)
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

`dev` uses a layered config system so you can set preferences once and have them apply everywhere. When you run `dev up` or `dev build`, these layers merge together to produce the final config.

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

If no base config exists, this layer is simply skipped. Pass `--no-base` to `dev up` or `dev build` to skip it for a single run.

The base config is merged in memory only — it is never written into the project's own config, and features it contributes are kept out of the project's `devcontainer-lock.json`. This holds for both a project with its own `.devcontainer/devcontainer.json` and a recipe-based one.

Recipe-based projects keep `recipe.json` as the durable source of truth. `dev up` and `dev build` compose the recipe in memory with the current base and runtime layers, so edits to `~/.dev/base/devcontainer.json` take effect on the next run without regenerating project state. `--no-base` stays local to that invocation.

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

When you bring a workspace up with `dev up` or build it with `dev build`, the config layers merge in this order (lowest to highest priority):

```
1. Global template     (~/.dev/global/<name>/...)
2. Base config         (~/.dev/base/devcontainer.json)
3. Runtime config      (~/.dev/<runtime>/devcontainer.json)
4. Per-project config  (.devcontainer/devcontainer.json or recipe)
```

The global template and runtime layers come from a recipe, so a project with its own `.devcontainer/devcontainer.json` merges just the base config beneath it.

Higher-priority layers override lower ones, with merge behavior depending on the field type:

| Field type | Merge strategy | Examples |
|-----------|----------------|----------|
| Scalar | Higher priority wins | `image`, `remoteUser` |
| Array | Concatenate (deduplicated) | `mounts`, `forwardPorts`, `runArgs` |
| Map | Merge (higher priority keys win) | `remoteEnv`, `containerEnv` |
| Features | Union (all features combined) | `features` |
| Lifecycle commands | Named-command objects union (higher priority wins per name); string and array forms follow scalar rules | `postCreateCommand`, `onCreateCommand` |

**Example:** if your global template sets `image: rust:latest` and your base config sets `remoteUser: vscode` with a zsh feature, the final config gets the Rust image, the vscode user, and both sets of features combined.

Whichever of `image`, `build`, or `dockerComposeFile` a project's own `.devcontainer/devcontainer.json` declares is authoritative: the competing selectors from the base layer are dropped rather than merged, so a base config `image` cannot turn a `build`- or compose-based project into an image-based one.

A recipe project has no `devcontainer.json`, so the same rule applies to the highest layer that declares a selector — the recipe's own overrides, then the runtime layer, then the base config. `dev config set image` on a recipe therefore drops a `build` inherited from the global template.

Command-line overrides such as `dev up --ports` are applied last, on top of the merged result.

### Derived images and disk use

When a config declares features, `dev up` and `dev build` layer them onto a derived image tagged `<folder>-features-<digest>`. The digest covers the effective config values that shape the image: the base image *selector* (`image`, or `build.dockerfile`/`context`/`args`), the declared `features`, `remoteUser`, `containerEnv`, and `remoteEnv`. Edit any of those and the next run builds a new image rather than reusing a stale one — which is what keeps a cached image from silently omitting base-config changes.

The digest covers selectors, not the files they point at. Editing a `Dockerfile` referenced by `build.dockerfile` leaves the digest unchanged, so the cached image is reused; pass `--rebuild` or `--no-cache` after changing Dockerfile contents.

The consequence is that superseded images are left behind rather than overwritten in place. `dev` does not delete them automatically, since it cannot tell which are still in use by stopped containers or other tooling. Reclaim the space with your runtime's own tooling when it starts to matter:

```sh
# List the derived images for a project
docker image ls --filter 'reference=<folder>-features-*'

# Remove images not referenced by any container
docker image prune
```

### Workspace vs user scope

`dev new` lets you choose where the config lives:

- **Workspace scope** — writes `.devcontainer/recipe.json` plus any template auxiliary files in the project (committed to git, shared with the team)
- **User scope** — writes a lightweight recipe to `~/.dev/devcontainers/<folder>/` (keeps the workspace clean, personal to you)

Recipes reference a global template by name and store any per-project overrides. The full config is composed at build/run time.

A recipe names its global template rather than copying it, so **each machine needs a global template of that name** in `~/.dev/global/`. On a fresh clone, run `dev new` and pick the same template to create it; `dev up` names the missing template and where it looked if it isn't there yet.

Because a recipe project has no `devcontainer.json` on disk, VS Code's "Reopen in Container" has nothing to read. Run `dev up` and attach your editor to the running container instead. `dev vscode repair` still re-links user-scoped projects that predate recipes and kept a real `devcontainer.json`; it refuses recipe projects rather than leaving a link that resolves to nothing.

## Local domain routing

`dev` integrates with [Caddy](https://caddyserver.com/) and dnsmasq to give each project a `.test` hostname (e.g. `appname.test`) so you never have to remember port numbers.

### One-time setup

Install and configure dnsmasq to resolve all `*.test` domains to localhost:

```sh
brew install dnsmasq
echo 'address=/.test/127.0.0.1' >> /opt/homebrew/etc/dnsmasq.conf
sudo brew services start dnsmasq
sudo mkdir -p /etc/resolver
echo 'nameserver 127.0.0.1' | sudo tee /etc/resolver/test
```

Install Caddy and start it with the dev-managed Caddyfile:

```sh
brew install caddy
sudo caddy start --config ~/.dev/caddy/Caddyfile
```

Caddy only needs to be started once — it persists across reboots and `dev` handles reloads automatically.

**Note:** After first-time DNS setup, flush your browser's DNS cache or it may not pick up `.test` resolution immediately.

- Chrome: visit `chrome://net-internals/#dns` and click **Clear host cache**
- macOS system cache: `sudo dscacheutil -flushcache && sudo killall -HUP mDNSResponder`

### How it works

When you run `dev up`, if `forwardPorts` is configured in your merged devcontainer config, `dev` will:

1. Write a Caddy config fragment to `~/.dev/caddy/sites/<appname>.caddy`
2. Signal Caddy to reload
3. Print the URL(s) your project is available at

```sh
dev up
# Container 'appname' is ready.
#   → https://appname.test → port 3000
```

When you run `dev down`, the Caddy fragment is removed and Caddy reloads.

The hostname is derived from your workspace folder name. Multiple `forwardPorts` entries get their own subdomains:

| forwardPorts   | Hostname                      |
| -------------- | ----------------------------- |
| `[3000]`       | `appname.test`                      |
| `[3000, 8080]` | `appname.test`, `appname-8080.test` |

### Caddy config files

| Path                              | Purpose                                 |
| --------------------------------- | --------------------------------------- |
| `~/.dev/caddy/Caddyfile`          | Root config, imports all site fragments |
| `~/.dev/caddy/sites/<name>.caddy` | Per-project fragment, managed by `dev`  |

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

**Note:** The Apple Containers runtime is opt-in — it is only available on macOS and only when built with `--features apple`:

```sh
cargo build --release --features apple
```

That build needs `protoc` on `PATH`; see [CONTRIBUTING.md](CONTRIBUTING.md#development) for the prerequisites.

## License

See [LICENSE](LICENSE).

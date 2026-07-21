# dev

`dev` is a terminal-first devcontainer CLI. It brings up a [devcontainer](https://containers.dev/) from a `devcontainer.json` and lets you drop into its shell from the terminal — no VS Code process required.

It exists because running an AI coding agent inside a devcontainer used to mean keeping VS Code open just to host the container. An agent does not need an IDE; it needs a shell. `dev` started as a way to get that shell: bring the container up, enter it, and let the agent run inside. Everything else the CLI does grew from that.

## How it got here

The project evolved in a few steps:

1. **Terminal-first container access** — `dev up` brings a container up from `devcontainer.json`; `dev shell` and `dev exec` run inside it without VS Code.
2. **Composable configuration** — a base config under `~/.dev/base`, reusable global templates, and per-runtime configs merge as layers, so preferences set once apply everywhere.
3. **Pluggable runtimes** — Docker and Podman are auto-detected today; Apple Containers is an opt-in, macOS-only path that is still evolving.
4. **VS Code attach** — `dev open` launches VS Code attached to the *same* running container, so the terminal-first flow and the IDE flow share one container instead of starting parallel ones.

## Install

```sh
cargo install devcontainer
```

The binary is named `dev`. From source:

```sh
cargo install --path .
```

## The terminal-first flow

The core loop is two commands. From a project with a `.devcontainer/devcontainer.json`:

```sh
# Bring the container up (builds the image if needed, starts the container)
dev up

# Drop into an interactive shell inside the running container
dev shell
```

`dev shell` opens an interactive shell as the configured `remoteUser`, in the workspace folder, with `REMOTE_CONTAINERS=true` set — the same environment VS Code's devcontainer integration would give you. It probes for `zsh`, `bash`, then `sh`; pass `--shell /bin/bash` to force one.

To run a one-off command instead of an interactive shell:

```sh
dev exec -- cargo test
dev exec -u root -- apt-get update
```

This is the shape an AI coding agent uses: `dev up` once, then `dev shell` (or `dev exec -- <agent command>`) to do its work inside the container. No editor is running.

If you have no `.devcontainer/` yet, scaffold one:

```sh
dev init                                  # minimal .devcontainer/ with a Dockerfile
dev new                                   # interactive: pick a template from the registry
dev new --template rust                   # by template id
```

`--template` takes the short id from the `ID` column of `dev list templates`, not a fully
qualified OCI reference.

## Bringing a container up with `dev up`

`dev up` resolves the effective config (merging the layers below), builds the derived image if features have changed, creates and starts the container, runs lifecycle hooks, and prints `Container '<name>' is ready.`

```sh
dev up                  # build if needed and start
dev up --rebuild        # rebuild the image even if it exists
dev up --no-cache       # rebuild without cache
dev up --no-base        # skip ~/.dev/base for this run
dev up --ports 3000     # override forwardPorts (host:container or just port)
dev up --buildkit       # BuildKit-optimized feature installation
```

`--frozen-lockfile` errors if `devcontainer-lock.json` is missing or its features don't match, for reproducible builds.

`--update-remote-user-uid-default` (`on` by default, also accepted by `dev build`) sets the fallback for `updateRemoteUserUID` when the config doesn't declare it: on Linux, `on` rebuilds the image with the `remoteUser`'s UID/GID remapped to yours so bind-mounted files stay writable. `never` disables the remap even when the config asks for it. It is a no-op on macOS, and when `remoteUser` is `root` or a numeric UID.

Once the container is ready, the terminal-first entry points all attach to that same running container:

```sh
dev shell               # interactive shell
dev exec -- cargo build # one command
dev status              # container state (add --json for machine-readable)
dev down                # stop (add --remove to delete the container)
```

## Attaching VS Code to the running container

`dev open` launches VS Code attached to the container that `dev up` already started — it does not start a second one. The terminal-first flow and the IDE flow share a single container.

```sh
dev up          # terminal-first: container is up
dev open        # VS Code attaches to that running container
dev open --insiders   # use VS Code Insiders instead
```

`dev open` builds a `vscode-remote://dev-container+…` URI from the workspace path. When the config lives outside the workspace (user-scoped or recipe-based), it embeds the composed `devcontainer.json` path so VS Code's devcontainer CLI gets `--config` pointed at the right file.

> **Recipes and "Reopen in Container":** a recipe-based project has no `devcontainer.json` on disk for VS Code to read, so the editor's "Reopen in Container" has nothing to open. Run `dev up` and `dev open` instead. `dev vscode repair` re-links *legacy* user-scoped projects that still keep a real `devcontainer.json`; it refuses recipe projects rather than leaving a link that resolves to nothing.

## Layered configuration

`dev` merges config layers so you set preferences once and they apply everywhere. Layers merge in this order, lowest to highest priority:

```
1. Global template     ~/.dev/global/<name>/.devcontainer/devcontainer.json
2. Base config         ~/.dev/base/devcontainer.json
3. Runtime config      ~/.dev/<runtime>/devcontainer.json
4. Project config      .devcontainer/devcontainer.json  (or a recipe — see below)
```

A project with its own `.devcontainer/devcontainer.json` merges just the base config beneath it (no global template or runtime layer). A **recipe** project composes all four layers at build/run time.

### Base config

`~/.dev/base/devcontainer.json` holds user-wide defaults applied to every container — preferred features, `remoteUser`, environment, etc.

```sh
dev base edit                                       # open in $EDITOR
dev base new                                        # interactive, with feature selection
dev base config set remoteUser vscode
dev base config add features ghcr.io/devcontainers/features/common-utils:2
dev base config add remoteEnv EDITOR=vim
```

If no base config exists, the layer is skipped. Pass `--no-base` to `dev up`/`dev build` to skip it for one run. The base layer is merged in memory only — it is never written into the project's own config, and features it contributes are kept out of the project's `devcontainer-lock.json`.

### Global templates

`~/.dev/global/<name>/` are reusable starter kits you save once and apply to any project — a "rust" template with your toolchain, a "node" template with your Node setup.

```sh
dev global new --name my-rust          # interactive: pick a registry template, options, features
dev global list
dev global edit my-rust                # open its config in $EDITOR
dev global config my-rust add features ghcr.io/devcontainers/features/node:1
dev global remove my-rust
```

### Recipes: workspace vs user scope

`dev new` lets you choose where the config lives:

- **Workspace scope** — writes `.devcontainer/recipe.json` (plus template aux files) into the project, committed and shared with the team.
- **User scope** — writes a lightweight recipe to `~/.dev/devcontainers/<folder>/`, keeping the workspace clean and personal to you.

A recipe references its global template **by name** and stores per-project overrides; the full config is composed at build/run time. Because the template is named rather than copied, **each machine needs a global template of that name** in `~/.dev/global/`. On a fresh clone, run `dev new` and pick the same template; `dev up` names the missing template and where it looked if it isn't there yet.

Recipe-based projects keep `recipe.json` as the durable source of truth. `dev up` and `dev build` compose the recipe in memory with the current base and runtime layers, so edits to `~/.dev/base/devcontainer.json` take effect on the next run without regenerating project state.

### How layers merge

Higher-priority layers override lower ones, with behavior depending on the field type:

| Field type | Merge strategy | Examples |
|-----------|----------------|----------|
| Scalar | Higher priority wins | `image`, `remoteUser`, `name` |
| Array | Concatenate (deduplicated) | `mounts`, `forwardPorts`, `runArgs` |
| Map | Merge (higher priority keys win) | `remoteEnv`, `containerEnv` |
| Features | Union (all features combined) | `features` |
| Lifecycle commands | Named-command objects union (higher priority wins per name); string and array forms follow scalar rules | `postCreateCommand`, `onCreateCommand` |

**Selector precedence.** Whichever of `image`, `build`, or `dockerComposeFile` a project's own `.devcontainer/devcontainer.json` declares is authoritative: competing selectors from lower layers are dropped, not merged — so a base config `image` cannot turn a `build`- or compose-based project into an image-based one. A recipe project has no `devcontainer.json`, so the same rule applies to the highest layer that declares a selector (recipe overrides, then runtime, then base). `dev config set image` on a recipe therefore drops a `build` inherited from the global template.

Command-line overrides such as `dev up --ports` are applied last, on top of the merged result.

### Derived images and disk use

When a config declares features, `dev up` and `dev build` layer them onto a derived image tagged `vsc-<folder>-<workspace-hash>-features-<digest>` — the base image name `dev` derives for the workspace, plus a `-features-` suffix. The digest covers the effective config values that shape the image: the base image *selector* (`image`, or `build.dockerfile`/`context`/`args`), the declared `features`, `remoteUser`, `containerEnv`, and `remoteEnv`. Edit any of those and the next run builds a new image rather than reusing a stale one — which is what keeps a cached image from silently omitting base-config changes.

The digest covers selectors, not the files they point at. Editing a `Dockerfile` referenced by `build.dockerfile` leaves the digest unchanged, so the cached image is reused; pass `--rebuild` or `--no-cache` after changing Dockerfile contents.

Superseded images are left behind rather than overwritten in place — `dev` does not delete them automatically, since it cannot tell which are still in use by stopped containers or other tooling. Reclaim space with your runtime's own tooling when it matters:

```sh
docker image ls --filter 'reference=vsc-*-features-*'
docker image prune
```

## Container runtimes

`dev` auto-detects a running runtime on startup. **Docker** and **Podman** are the supported, auto-detected runtimes today. If both are running, Podman is preferred; if neither is, `dev` prints a targeted hint (start Docker Desktop, or `podman machine start`).

```sh
dev up                       # auto-detect
dev up --runtime podman      # force a runtime
dev up --runtime docker
```

**Apple Containers** is an opt-in, macOS-only runtime that talks to the native container stack over XPC rather than the Docker API. It is **not** auto-detected: you must build with the `apple` feature and pass `--runtime apple` explicitly. It is still evolving — compose-based configs and some flows route through the Docker/Podman compose CLI and are not available under it.

```sh
# build with Apple Containers support (macOS, needs protoc on PATH)
cargo build --release --features apple

dev up --runtime apple
```

Docker Compose (`dockerComposeFile`) is supported for the full lifecycle — build, up, down, shell, exec, features, UID remapping — and uses `docker compose` or `podman compose` depending on the selected runtime.

## Local `.test` domains

`dev` integrates with [Caddy](https://caddyserver.com/) and dnsmasq to give each project a `.test` hostname (e.g. `appname.test`) so you don't memorize port numbers.

### One-time setup

```sh
brew install dnsmasq
echo 'address=/.test/127.0.0.1' >> /opt/homebrew/etc/dnsmasq.conf
sudo brew services start dnsmasq
sudo mkdir -p /etc/resolver
echo 'nameserver 127.0.0.1' | sudo tee /etc/resolver/test

brew install caddy
sudo caddy start --config ~/.dev/caddy/Caddyfile
```

Caddy only needs to be started once — it persists across reboots and `dev` handles reloads. After first-time DNS setup, flush your browser/system DNS cache or `.test` may not resolve immediately:

```sh
sudo dscacheutil -flushcache && sudo killall -HUP mDNSResponder
```

### How it works

When you run `dev up` and `forwardPorts` is set in the merged config, `dev` writes a Caddy fragment to `~/.dev/caddy/sites/<appname>.caddy`, signals Caddy to reload, and prints the URL(s):

```sh
dev up
# Container 'appname' is ready.
#   → https://appname.test → localhost:3000
```

`dev down` removes the fragment and reloads Caddy. The hostname comes from the workspace folder name. Multiple `forwardPorts` get their own subdomains:

| `forwardPorts`   | Hostnames                              |
| ---------------- | -------------------------------------- |
| `[3000]`         | `appname.test`                         |
| `[3000, 8080]`   | `appname.test`, `appname-8080.test`    |

For ad-hoc forwarding (a port not in `forwardPorts`, or a custom subdomain like `admin.appname.test`), use `dev forward`:

```sh
dev forward 3000                            # forward 3000 → 3000
dev forward 8080:3000                       # host 8080 → container 3000
dev forward 3000 --name admin.appname.test  # custom .test subdomain
dev forward 3000 --keepalive 30s
dev forward 3000 --stop                     # stop a forwarder
dev forward 3000 --list                     # list this workspace's forwarders
```

`--name` is used verbatim as the Caddy site hostname, so include the `.test` suffix — a name
without it won't resolve through the dnsmasq `.test` resolver. `--list` reports every forwarder
for the workspace, but the port argument is still required by the CLI (it is ignored).

| Path                              | Purpose                                 |
| --------------------------------- | --------------------------------------- |
| `~/.dev/caddy/Caddyfile`          | Root config, imports all site fragments |
| `~/.dev/caddy/sites/<name>.caddy` | Per-project fragment, managed by `dev`  |

## Command reference

```sh
dev init                                     # minimal .devcontainer/ with a Dockerfile
dev new [--template <id>] [--options <k=v>…] # .devcontainer/ from a registry template

dev build [--tag <t>] [--no-cache] [--buildkit] [--no-base] [--frozen-lockfile]
          [--update-remote-user-uid-default never|on|off]
dev up    [--rebuild] [--no-cache] [--buildkit] [--no-base] [--ports …] [--frozen-lockfile]
          [--update-remote-user-uid-default never|on|off]
dev down  [--remove]
dev shell [--shell /bin/bash]
dev exec  [-u <user>] -- <cmd>…

dev status [--json]
dev open   [--insiders]

dev list templates [-q <query>] [--json] [--refresh]
dev list features  [-q <query>] [--json] [--refresh]

dev config set   <property> <value>
dev config add   <property> <value>     # features, forwardPorts, remoteEnv, mounts…
dev config unset <property>
dev config remove <property> <value>
dev config list

dev global new  [--name <n>] [--template <id>]
dev global list
dev global edit <name>
dev global remove <name>
dev global config <name> set|add|unset|remove|list …

dev base new
dev base edit
dev base config set|add|unset|remove|list …

dev forward <port> [--name <host>] [--keepalive <dur>] [-d] [--stop] [--list]

dev vscode repair              # re-link a legacy user-scoped devcontainer.json for VS Code
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
# binary at target/release/dev
```

The Apple Containers runtime is opt-in — macOS only, and only when built with `--features apple` (needs `protoc` on `PATH`; see [CONTRIBUTING.md](CONTRIBUTING.md#development) for prerequisites):

```sh
cargo build --release --features apple
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for test, lint, and style gates.

## License

See [LICENSE](LICENSE).
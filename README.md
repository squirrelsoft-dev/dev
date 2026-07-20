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

For a recipe there is no such project file, so the *highest layer that declares a selector* wins: recipe overrides, then runtime, then base, then the global template. `dev config set image` on a recipe therefore drops a `build` inherited from its global template.

Higher layers override lower ones, with the strategy depending on the field type:

| Field type | Merge strategy | Examples |
|-----------|----------------|----------|
| Scalar | Higher priority wins | `image`, `remoteUser` |
| Array | Concatenate (deduplicated) | `mounts`, `forwardPorts`, `runArgs` |
| Map | Merge (higher priority keys win) | `remoteEnv`, `containerEnv` |
| Features | Union | `features` |
| Lifecycle commands | Named-command objects union per name; string and array forms follow scalar rules | `postCreateCommand`, `onCreateCommand` |

Command-line overrides such as `dev up --ports` are applied last, on top of the merged result.

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

Reusable devcontainer setups you save once and apply anywhere. A "rust" template with your toolchain, a "node" template with your Node setup. A recipe references its template by name, so each machine needs a template of that name in `~/.dev/global/` — a fresh clone fails until one exists. Recreate it with `dev new`, picking the same template and scope; note that this rewrites `recipe.json` and drops any `dev config set` overrides it carried. To keep those, point `globalTemplate` in `recipe.json` at a template you already have instead.

```sh
dev global new --name my-rust
dev global list
dev global edit my-rust
dev global remove my-rust
```

### Recipe projects

A recipe names a global template and stores overrides. It has no `devcontainer.json` on disk — `dev up` and `dev build` compose it in memory with the current base and runtime layers. Editing `~/.dev/base/devcontainer.json` takes effect on the next run without regenerating project state.

Those same commands do write the template's auxiliary files — Dockerfiles, compose files, scripts — into `.devcontainer/` so the build has the context it references. They are filled in only where missing and are not recorded in `recipe.json`, so a later `dev new` that wants to replace one refuses rather than assuming it is safe to overwrite.

Because a recipe project has no `devcontainer.json`, VS Code's "Reopen in Container" has nothing to read. Run `dev up` and attach your editor to the running container instead. `dev vscode repair` re-links *legacy* user-scoped projects that predate recipes and kept a real `devcontainer.json`; it refuses recipe projects rather than leaving a link that resolves to nothing.

## Container runtimes

`dev` supports three runtimes, selected by `--runtime` or auto-detected:

- **Docker** — used when it is the only one running
- **Podman** — drop-in compatible, no daemon; preferred when both it and Docker are running
- **Apple Containers** — macOS native via XPC; never auto-detected, so pass `--runtime apple`, and requires a source build with `--features apple` (see [Installation](#installation))

Pass `--runtime docker` to pin the runtime when both Docker and Podman are up.

Docker Compose projects get a full lifecycle path: build, layer features, UID remap, generate a compose override injecting labels/env/mounts/ports, start services, run lifecycle hooks.

## VS Code integration

Optional. Attach your editor to a running container:

```sh
dev open
dev open --insiders
```

Recipe projects have no `devcontainer.json` for the remote-containers extension to open, so attaching to a running container is the supported flow — see [Recipe projects](#recipe-projects) for `dev vscode repair` and the legacy user-scoped case.

## Local domain routing

Each project gets a `.test` hostname (`appname.test`) via Caddy and dnsmasq. Install both once and point the resolver at localhost:

```sh
brew install dnsmasq caddy
echo 'address=/.test/127.0.0.1' >> /opt/homebrew/etc/dnsmasq.conf
sudo brew services start dnsmasq
sudo mkdir -p /etc/resolver
echo 'nameserver 127.0.0.1' | sudo tee /etc/resolver/test
```

When the merged config declares `forwardPorts`, `dev up` writes `~/.dev/caddy/Caddyfile` and a per-project fragment, reloads Caddy, and prints the URL:

```sh
dev up
# Container 'vsc-appname-<hash>' is ready.
#   → https://appname.test → localhost:3000
```

`dev` creates that Caddyfile lazily on that first forwarded-port run, so start Caddy against it afterwards — not before:

```sh
sudo caddy start --config ~/.dev/caddy/Caddyfile
```

Those `forwardPorts` runs are also where `dev up` checks your host setup: it prints the Caddy install steps when `caddy` isn't on `PATH`, and the dnsmasq steps when `/etc/resolver/test` is missing. With no `forwardPorts` configured it does neither. After first-time DNS setup, flush your browser's DNS cache (Chrome: `chrome://net-internals/#dns` → **Clear host cache**) or `.test` may not resolve immediately.

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

Global flags: `--workspace <path>`, `--runtime <docker|podman|apple>`, `-v` (verbose build and registry output).

## Derived images and disk use

When a config declares features, `dev up` and `dev build` layer them onto a derived image named `vsc-<folder>-<workspace-hash>-features-<digest>`. The digest covers the effective config values — base image selector, features, `remoteUser`, `containerEnv`, `remoteEnv`. Edit any of those and the next run builds a new image rather than reusing a stale one.

The digest covers selectors, not the files they point at. Editing a `Dockerfile` referenced by `build.dockerfile` leaves the digest unchanged; pass `--rebuild` or `--no-cache` after changing Dockerfile contents.

Superseded images accumulate rather than being overwritten in place. Reclaim space with your runtime's own tooling:

```sh
docker image ls --filter 'reference=vsc-<folder>-*-features-*'
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

The Apple Containers runtime is behind an optional `apple` feature, so `--runtime apple` only works on a binary built with it:

```sh
cargo install --path . --features apple
```

That build is macOS-only and needs `protoc` on `PATH`; see [CONTRIBUTING.md](CONTRIBUTING.md#development) for the prerequisites.

## Safety

`dev` manages containers and interacts with container runtimes and OCI registries. Security concerns include container escape, unsafe config handling, credential exposure, and host filesystem operations. See [SECURITY.md](SECURITY.md) for reporting a vulnerability.

## Contributing

Fork, clone, branch, and submit a PR against `main`. Install the Rust toolchain (stable), run `cargo test` and `cargo clippy` before submitting, and keep commits focused. See [CONTRIBUTING.md](CONTRIBUTING.md) for full details.

## License

MIT — see [LICENSE](LICENSE).

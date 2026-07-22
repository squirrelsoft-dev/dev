# Project Commands
- Install: `cargo build`
- Build: `cargo build --release`
- Test: `cargo test`
- Lint: `cargo clippy --all-targets -- -D warnings`
- Format check: `cargo fmt --all --check`
- Typecheck: `cargo check`

# Non-Negotiables
- Do not add new dependencies without a strong reason
- Always include verification steps after code changes
- Run tests before marking any task complete

# Common Mistakes
- (add rules here as you discover repeated issues)

# Learnings
- The `devcontainer` crate's tests are all in-file `#[cfg(test)] mod tests`; it has no `tests/`
  directory (the vendored `crates/buildkit-client-patch` is the exception).
- Anything that reads `~/.dev` has an `*_in` variant taking a `&DevHome` (defined in
  `src/util/paths.rs`); test against those with `DevHome::at(tmp)` rather than the
  `current()`-based wrappers.
- In `src/devcontainer/merge.rs`, array merge strategy is split: `forwardPorts`/`mounts`
  concatenate with dedup (`merge_array`), but `runArgs` concatenates **without** dedup
  (`merge_array_concat`) because repeated flags like `--env-file` are legitimate and
  order matters. Don't move `runArgs` back into the dedup path.
- `runArgs` is translated in `src/devcontainer/run_args.rs` (env subset only:
  `--env-file`/`--env`/`-e`) and merged into `ContainerConfig.env` in `src/commands/up.rs`
  after `containerEnv`; every other flag is rejected before container creation. Both Docker
  and Podman send the same bollard create body (`BollardRuntime::to_create_body`); Podman
  wraps `BollardRuntime`. `extra_args` on `ContainerConfig` is now always empty (kept for
  struct compatibility).

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.

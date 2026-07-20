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
- Tests are in-file `#[cfg(test)] mod tests`; there is no `tests/` directory.
- Anything that reads `~/.dev` takes a `&DevHome` in an `*_in` variant (`src/util/paths.rs`);
  test against those with `DevHome::at(tmp)` rather than the `current()`-based wrappers.
- `cargo fmt --check` is dirty repo-wide on `main`, so it is not a useful signal.
  Check that a change adds no *new* rustfmt diffs instead of expecting a clean run.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.

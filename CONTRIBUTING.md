# Contributing to dev

Thanks for your interest in contributing! Here's how to get started.

## Getting Started

1. Fork the repository
2. Clone your fork and create a branch:
   ```sh
   git clone https://github.com/<your-username>/dev.git
   cd dev
   git checkout -b my-feature
   ```
3. Install the Rust toolchain (stable): https://rustup.rs

## Development

```sh
# Build
cargo build

# Run tests
cargo test

# Lint
cargo clippy

# Type check
cargo check
```

**Note:** The `apple-container` crate requires macOS to compile. On other platforms, builds will exclude it automatically.

## Making Changes

- Keep commits focused — one logical change per commit
- Write descriptive commit messages explaining *why*, not just *what*
- Add tests for new functionality
- Run `cargo test` and `cargo clippy` before submitting

## Pull Requests

1. Push your branch to your fork
2. Open a pull request against `main`
3. Describe what you changed and why
4. Link any related issues

PRs should:
- Pass all existing tests
- Include tests for new behavior
- Have no clippy warnings
- Follow the existing code style

## Reporting Bugs

Open an issue with:
- What you expected to happen
- What actually happened
- Steps to reproduce
- Your OS, Rust version, and container runtime

## Suggesting Features

Open an issue describing the use case and proposed behavior. Discussion before implementation helps avoid wasted effort.

## Code Style

- Functions should be no more than 50 lines — break larger ones into helpers
- Single responsibility per function and module
- Use descriptive names (`calculate_invoice_total` not `do_calc`)
- No commented-out code or debug statements in PRs
- Never swallow errors silently

## License

By contributing, you agree that your contributions will be licensed under the [MIT License](LICENSE).

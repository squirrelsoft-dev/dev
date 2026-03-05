# BuildKit Rust Client

<div align="center">

[![Crates.io](https://img.shields.io/crates/v/buildkit-client)](https://crates.io/crates/buildkit-client)
[![Documentation](https://img.shields.io/docsrs/buildkit-client)](https://docs.rs/buildkit-client)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue)](#license)
[![Rust Version](https://img.shields.io/badge/rust-1.70%2B-orange)](https://www.rust-lang.org)
<!-- [![Build Status](https://img.shields.io/github/actions/workflow/status/corespeed-io/buildkit-client/ci.yml)](https://github.com/corespeed-io/buildkit-client/actions) -->
<!-- [![codecov](https://img.shields.io/codecov/c/github/corespeed-io/buildkit-client)](https://codecov.io/gh/corespeed-io/buildkit-client) -->

A full-featured Rust client library and CLI for interacting with [moby/buildkit](https://github.com/moby/buildkit) to build container images via gRPC.

[Features](#features) •
[Installation](#installation) •
[Quick Start](#quick-start) •
[Usage](#usage) •
[Documentation](#documentation) •
[Contributing](#contributing)

</div>

---

## Features

- **Complete gRPC Implementation** - Direct integration with BuildKit's gRPC API
- **Multiple Build Sources** - Support for local Dockerfiles and GitHub repositories
- **Authentication Support** - GitHub private repositories and Docker Registry authentication
- **Advanced Build Options** - Build args, target stages, multi-platform builds
- **Real-time Progress** - Live build progress and log streaming
- **Cache Management** - Support for cache import/export
- **Registry Push** - Automatic push of built images to registries
- **Session Protocol** - Full implementation of BuildKit's bidirectional session protocol
- **HTTP/2 Tunneling** - HTTP/2-over-gRPC for file synchronization

## Prerequisites

- Rust 1.70+
- Docker or BuildKit daemon
- Git (for fetching proto files)

## Installation

### As a Library

Add to your `Cargo.toml`:

```toml
[dependencies]
buildkit-client = "0.1" # or bkit if you like
tokio = { version = "1", features = ["full"] }
anyhow = "1.0"
```

### As a CLI Tool

```bash
git clone https://github.com/corespeed-io/buildkit-client.git
cd buildkit-client
cargo install --path .
```

Proto files are automatically managed during build - no manual setup required.

## Quick Start

See [Usage Guide](./docs/USAGE.md) for detailed CLI and library usage examples.

## Project Structure

```
buildkit-client/
├── src/
│   ├── main.rs          # CLI tool entry point
│   ├── lib.rs           # Library entry point
│   ├── client.rs        # BuildKit gRPC client
│   ├── builder.rs       # Build configuration
│   ├── solve.rs         # Build execution logic
│   ├── progress.rs      # Progress handling
│   ├── session/         # Session protocol implementation
│   │   ├── mod.rs       # Session lifecycle & metadata
│   │   ├── grpc_tunnel.rs  # HTTP/2-over-gRPC tunnel
│   │   ├── filesync.rs  # File synchronization
│   │   └── auth.rs      # Registry authentication
│   └── proto.rs         # Protobuf generated code
├── proto/               # BuildKit protobuf definitions
├── examples/            # Sample Dockerfiles
├── tests/               # Comprehensive test suite
├── docker-compose.yml   # Test environment setup
└── README.md
```

## BuildKit gRPC API

This project directly uses BuildKit's gRPC API:

- `Control.Solve` - Execute build operations
- `Control.Status` - Stream build status updates
- `Control.Info` - Get BuildKit information
- `Control.Session` - Bidirectional session stream

All protobuf definitions are fetched from the [moby/buildkit](https://github.com/moby/buildkit) repository.

## Documentation

- **[Quick Start Guide](./docs/QUICK_START.md)** - Get up and running quickly
- **[Usage Guide](./docs/USAGE.md)** - CLI and library usage examples with configuration options
- **[Architecture Guide](./docs/ARCHITECTURE.md)** - Complete architecture and protocol documentation
- **[Development Guide](./docs/DEVELOPMENT.md)** - Development workflows, testing, and proto management
- **[Testing Guide](./docs/TESTING.md)** - Complete testing documentation (unit, integration, GitHub builds)
- **[Implementation Notes](./CLAUDE.md)** - Detailed implementation notes for contributors

## Troubleshooting

### BuildKit Connection Failed

```bash
# Check if BuildKit is running
docker-compose ps

# View BuildKit logs
docker-compose logs buildkitd

# Restart services
docker-compose restart
```

### Registry Push Failed

Ensure the registry allows insecure connections (for localhost):

```yaml
# docker-compose.yml
services:
  buildkitd:
    environment:
      - BUILDKIT_REGISTRY_INSECURE=true
```

### Proto Compilation Errors

If you encounter protobuf compilation errors:

```bash
# Force rebuild (will redownload proto files)
cargo clean
PROTO_REBUILD=true cargo build

# Or use clone mode if download fails
PROTO_FETCH_MODE=clone cargo build
```

Proto files are now automatically managed by `build.rs`. See [Development Guide](./docs/DEVELOPMENT.md) for details.

## Development

For detailed development workflows, testing strategies, and proto management, see [Development Guide](./docs/DEVELOPMENT.md).

Quick commands:
```bash
make build         # Build project
make test          # Run tests
make up            # Start docker-compose services
cargo fmt          # Format code
cargo clippy       # Run linter
```

## Architecture

This project implements a complete BuildKit gRPC client with:
- Bidirectional gRPC streaming for real-time communication
- HTTP/2-over-gRPC tunneling for BuildKit callbacks
- DiffCopy protocol for efficient file synchronization
- Session management with proper metadata handling
- Registry authentication support

For detailed architecture documentation, see [Architecture Guide](./docs/ARCHITECTURE.md).

## License

This project is dual-licensed under MIT OR Apache-2.0.

## Acknowledgments

- [moby/buildkit](https://github.com/moby/buildkit) - BuildKit project
- [tonic](https://github.com/hyperium/tonic) - Rust gRPC library
- [prost](https://github.com/tokio-rs/prost) - Protocol Buffers implementation
- [h2](https://github.com/hyperium/h2) - HTTP/2 implementation

## Contributing

Contributions are welcome! Please feel free to submit Issues and Pull Requests.

Before submitting a PR:
1. Run `cargo fmt` and `cargo clippy`
2. Ensure all tests pass: `cargo test`
3. Add tests for new features
4. Update documentation as needed

## Related Links

- [BuildKit Documentation](https://github.com/moby/buildkit/tree/master/docs)
- [BuildKit API Reference](https://github.com/moby/buildkit/tree/master/api)
- [Docker Buildx](https://github.com/docker/buildx)
- [Container Image Specification](https://github.com/opencontainers/image-spec)

---

<div align="center">

**[⬆ back to top](#buildkit-rust-client)**

Made with ❤️ by AprilNEA

</div>

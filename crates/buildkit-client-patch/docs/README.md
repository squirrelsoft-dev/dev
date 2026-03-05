# BuildKit Client Documentation

Welcome to the BuildKit Client documentation! This directory contains comprehensive guides for using, testing, and developing with buildkit-client.

## 📚 Documentation Structure

```
docs/
├── README.md                   # This file
├── QUICK_START.md             # Quick start guide
├── PROTO_SETUP.md             # Proto file management
├── TESTING.md                 # Complete testing guide
└── GRPC_TUNNEL_ARCHITECTURE.md # gRPC tunnel architecture
```

## 🚀 Getting Started

New to buildkit-client? Start here:

1. **[Quick Start Guide](./QUICK_START.md)** - Get up and running in 3 steps
2. **[Proto Setup](./PROTO_SETUP.md)** - Understand proto file management
3. **[Main README](../README.md)** - Complete feature overview

## 🧪 Testing

Learn how to test buildkit-client:

- **[Testing Guide](./TESTING.md)** - Complete testing documentation
  - Unit tests (39 tests)
  - Integration tests (14+ tests)
  - GitHub repository builds (public and private)
  - Benchmarks and performance testing
  - Test utilities and fixtures
  - Running tests, debugging, CI/CD integration
  - Troubleshooting common issues

## 🔧 Development

For developers working on buildkit-client:

- **[Development Guide](../CLAUDE.md)** - Architecture and development guide
  - Essential commands
  - Architecture overview
  - High-level data flow
  - Session protocol details
  - HTTP/2 tunneling
  - DiffCopy file sync protocol
  - Common issues and solutions

## 📖 Additional Resources

### External Documentation
- [BuildKit Documentation](https://github.com/moby/buildkit/tree/master/docs)
- [BuildKit API Reference](https://github.com/moby/buildkit/tree/master/api)
- [Docker Buildx](https://github.com/docker/buildx)

### Project Files
- [CHANGELOG.md](../CHANGELOG.md) - Project changelog
- [Cargo.toml](../Cargo.toml) - Dependencies and configuration
- [docker-compose.yml](../docker-compose.yml) - Test environment setup

## 🎯 Quick Links by Task

### I want to...

#### Install and Run
→ [Quick Start Guide](./QUICK_START.md)

#### Build from Local Dockerfile
→ [Main README - Usage Section](../README.md#usage)

#### Build from GitHub Repository
→ [Testing Guide - GitHub Tests](./TESTING.md#github-repository-tests)

#### Write Tests
→ [Testing Guide - Writing Tests](./TESTING.md#writing-tests)

#### Understand the Architecture
→ [Development Guide](../CLAUDE.md)

#### Troubleshoot Issues
→ [Main README - Troubleshooting](../README.md#troubleshooting)

#### Contribute
→ [Main README - Contributing](../README.md#contributing)

## 📝 Documentation Guidelines

When adding new documentation:

1. **Follow the structure** - Place docs in appropriate subdirectories
2. **Use clear headings** - Make content easy to scan
3. **Include examples** - Show, don't just tell
4. **Cross-reference** - Link to related documentation
5. **Keep updated** - Update docs when code changes

## 🤝 Contributing to Documentation

Found an error or want to improve the docs? Contributions are welcome!

1. Check existing documentation for similar content
2. Follow the markdown formatting style used in existing docs
3. Test all code examples to ensure they work
4. Update cross-references if you move or rename files
5. Submit a pull request with your changes

## 📧 Getting Help

- **Issues**: [GitHub Issues](https://github.com/corespeed-io/buildkit-client/issues)
- **Discussions**: [GitHub Discussions](https://github.com/corespeed-io/buildkit-client/discussions)

---

<div align="center">

**Happy Building!** 🏗️

[⬆ Back to Main README](../README.md)

</div>

# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability, please report it responsibly. **Do not open a public issue.**

Instead, email the maintainers directly or use [GitHub's private vulnerability reporting](https://github.com/squirrelsoft-dev/dev/security/advisories/new).

Please include:
- A description of the vulnerability
- Steps to reproduce
- Potential impact
- Suggested fix (if any)

We will acknowledge receipt within 48 hours and aim to provide a fix or mitigation plan within 7 days.

## Scope

This project manages containers and interacts with container runtimes (Docker, Podman, Apple Containers) and OCI registries. Security concerns include but are not limited to:

- Container escape or privilege escalation
- Unsafe handling of user-supplied configuration
- OCI registry credential exposure
- Command injection via devcontainer config values
- Unsafe file operations on the host filesystem

## Supported Versions

Security fixes are applied to the latest release only.

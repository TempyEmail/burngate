# Contributing to Burngate

Thanks for your interest in contributing! This project is sponsored by [tempy.email](https://tempy.email) and welcomes contributions from the community.

## Getting started

1. Fork the repository
2. Clone your fork: `git clone https://github.com/YOUR_USERNAME/burngate.git`
3. Create a branch: `git checkout -b my-feature`
4. Make your changes
5. Run tests: `cargo test`
6. Run clippy: `cargo clippy -- -D warnings`
7. Run fmt: `cargo fmt --check`
8. Commit and push
9. Open a pull request

## Development setup

You need:
- Rust 1.70+ (`rustup update stable`)
- Redis (for integration tests): `docker run -d -p 6379:6379 redis:7-alpine`

```bash
# Build
cargo build

# Run tests
cargo test

# Run with debug logging
RUST_LOG=debug REDIS_HOST=127.0.0.1 BACKEND_SMTP=127.0.0.1:2525 cargo run

# Check for lint issues
cargo clippy -- -D warnings

# Format code
cargo fmt
```

## What to work on

- Check [open issues](https://github.com/TempyEmail/burngate/issues) for things tagged `good first issue` or `help wanted`
- Performance improvements are always welcome
- Documentation improvements
- Additional tests

## Code style

- Run `cargo fmt` before committing
- Run `cargo clippy -- -D warnings` and fix all warnings
- Write tests for new functionality
- Keep functions focused and small
- Use structured logging (`tracing` macros) with descriptive messages
- Follow existing patterns in the codebase

## Commit messages

- Use the imperative mood: "Add feature" not "Added feature"
- Keep the first line under 72 characters
- Reference issues when relevant: "Fix #123"

## Pull requests

- Keep PRs focused on a single change
- Update documentation if your change affects configuration or behavior
- Add tests for new features
- Make sure CI passes (tests, clippy, fmt)

## Reporting bugs

Open an issue with:
- What you expected to happen
- What actually happened
- Steps to reproduce
- Your environment (OS, Rust version, Redis version)

## Security

If you find a security vulnerability, please report it responsibly. See [SECURITY.md](SECURITY.md).

## License

By contributing, you agree that your contributions will be licensed under the MIT License.

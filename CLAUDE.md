# CLAUDE.md

Guidance for Claude Code when working on this project.

## Build & Run

```bash
# Build
cargo build

# Build release
cargo build --release

# Run tests
cargo test

# Run clippy
cargo clippy -- -D warnings

# Format
cargo fmt

# Run locally (needs Redis on localhost:6379)
RUST_LOG=debug REDIS_HOST=127.0.0.1 BACKEND_SMTP=127.0.0.1:2525 cargo run

# Docker build
docker build -t burngate .

# Docker run
docker run -e REDIS_HOST=host.docker.internal -e BACKEND_SMTP=host.docker.internal:2525 -p 25:25 burngate
```

## Architecture

Single-binary async Rust SMTP gateway. Sits on port 25 in front of a backend mail server. Checks Redis for mailbox existence at RCPT TO (before DATA) to reject spam early.

### Source layout

```
src/
  main.rs      - Entry point: Redis connection, TLS setup, TCP listener, metrics task
  config.rs    - Config struct loaded from environment variables
  session.rs   - SMTP state machine (EHLO, MAIL FROM, RCPT TO, DATA, STARTTLS, etc.)
  lookup.rs    - Redis mailbox existence checks (mb:{addr} key + addresses set)
  relay.rs     - SMTP relay to backend server
  tls.rs       - STARTTLS support via rustls
```

### Key design decisions

- **Hand-rolled SMTP protocol**: No external SMTP crate. The protocol up to DATA is simple (~15 commands). Avoids dependency bloat.
- **BufReader<TcpStream> for STARTTLS**: Reads through buffered reader, writes via `get_mut()`. SMTP is half-duplex so no concurrent read/write needed. On STARTTLS, `into_inner()` recovers the raw stream for TLS handshake.
- **Generic smtp_loop**: The main loop is generic over any `AsyncRead + AsyncWrite + Unpin` stream. Called twice: once for plain text, once for TLS.
- **Two-tier Redis check**: First checks `mb:{address}` (active, has TTL), then falls back to `addresses` set (permanent). Fail-closed on Redis errors.
- **Subdomain wildcard**: `abc.tempy.email` matches if `tempy.email` is in accepted domains.

### Redis key format

| Key | Type | Purpose |
|-----|------|---------|
| `mb:{address}` | String with TTL | Active mailbox (primary check) |
| `addresses` | Set | All ever-created addresses (fallback check) |

### Structured logging tags

- `[RCPT-ACCEPTED]` - mailbox verified
- `[MAIL-REJECTED]` - unknown address or domain
- `[MAIL-RELAYED]` - forwarded to backend
- `[RELAY-ERROR]` - backend relay failed
- `[METRICS]` - periodic counters (every 60s)

## Conventions

- All email addresses are lowercased before Redis lookup
- SMTP commands are matched case-sensitively (uppercase per RFC 5321)
- Connection timeout defaults to 300s
- Max message size defaults to 10MB
- JSON structured logging via `tracing` + `tracing-subscriber`
- No panics in production paths -- errors are logged and connections are dropped gracefully

## Testing

```bash
cargo test           # Unit tests
cargo clippy         # Lint
cargo fmt --check    # Formatting
```

Unit tests cover: `parse_command`, `extract_address`, `is_domain_accepted`, domain matching edge cases.

## Sponsor

This project is sponsored by [tempy.email](https://tempy.email).

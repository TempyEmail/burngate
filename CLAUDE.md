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
cargo clippy --all-targets -- -D warnings

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
  relay.rs     - SMTP relay to backend server (dot-stuffing, multi-line replies)
  ratelimit.rs - Per-IP rate + concurrency limits (IPv6 grouped by /64)
  tls.rs       - STARTTLS support via rustls, hot-reloadable certificate
  session/tests.rs - End-to-end session tests (fake lookup + fake backend over duplex streams)
```

### Key design decisions

- **Hand-rolled SMTP protocol**: No external SMTP crate. The protocol up to DATA is simple (~15 commands). Avoids dependency bloat.
- **BufReader<TcpStream> for STARTTLS**: Reads through buffered reader, writes via `get_mut()`. SMTP is half-duplex so no concurrent read/write needed. On STARTTLS, `into_inner()` recovers the raw stream for TLS handshake.
- **Generic smtp_loop**: The main loop is generic over any `AsyncRead + AsyncWrite + Unpin` stream. Called twice: once for plain text, once for TLS.
- **Two-tier Redis check**: First checks `mb:{address}` (active, has TTL), then falls back to `addresses` set (permanent). Fail-closed on Redis errors, but with `451` (temporary), never `550`, so senders retry.
- **Lookup trait**: `smtp_loop` is generic over `MailboxCheck`, so tests use an in-memory lookup instead of Redis.
- **DATA handling**: `read_data` always reads through the terminating `.` (even when rejecting), bounds memory by `MAX_MESSAGE_SIZE` regardless of newlines, dot-unstuffs and normalizes to CRLF; `relay::encode_data` re-stuffs. Bare CR is rejected. This keeps the backend from ever seeing an ambiguous end-of-data (SMTP smuggling).
- **Received header**: added to every relayed message, carrying the real client IP.
- **Subdomain wildcard**: `abc.tempy.email` and `a.b.tempy.email` match if `tempy.email` is in accepted domains.
- **Shutdown**: SIGTERM/Ctrl-C stops accepting and drains sessions for `SHUTDOWN_TIMEOUT`. SIGHUP reloads the TLS cert.
- **OpenTelemetry**: Optional OTLP tracing via `OTEL_EXPORTER_OTLP_ENDPOINT`. When set, `smtp.session` and `smtp.relay` spans are emitted. The relay step injects a W3C `traceparent` MIME header into outgoing emails so downstream mail processors can continue the trace. Zero overhead when the env var is unset.

### Redis key format

| Key | Type | Purpose |
|-----|------|---------|
| `mb:{address}` | String with TTL | Active mailbox (primary check) |
| `addresses` | Set | All ever-created addresses (fallback check) |

### Structured logging tags

- `[RCPT-ACCEPTED]` - mailbox verified
- `[MAIL-REJECTED]` - unknown address or domain, or bare CR in message
- `[MAIL-DEFERRED]` - lookup failed (Redis error/timeout), answered `451`
- `[MAIL-RELAYED]` - forwarded to backend
- `[RELAY-ERROR]` - backend relay failed
- `[METRICS]` - periodic counters (every 60s)

## Conventions

- All email addresses are lowercased before Redis lookup
- SMTP commands are matched case-insensitively (uppercased by `parse_command`, per RFC 5321)
- Command order is enforced: HELO/EHLO before MAIL, MAIL before RCPT/DATA
- Session timeout defaults to 300s total, idle timeout 60s per read
- Max message size defaults to 10MB
- JSON structured logging via `tracing` + `tracing-subscriber`
- No panics in production paths -- errors are logged and connections are dropped gracefully

## Documentation

After a successful build and before every commit:
- Run `cargo fmt` — CI enforces formatting, commits with fmt violations will fail
- Run `cargo clippy --all-targets -- -D warnings` — CI enforces no warnings
- CI also runs `cargo audit`; Actions are pinned by SHA and Docker base images by digest (Dependabot bumps both)
- Review and update `README.md`, `CLAUDE.md`, and `llms.txt` if needed

## Testing

```bash
cargo test           # Unit tests
cargo clippy         # Lint
cargo fmt --check    # Formatting
```

Unit tests cover: `parse_command`, `extract_address`, `is_domain_accepted`, rate/concurrency limiters, bounded `read_line`/`read_data`, dot-stuffing. `src/session/tests.rs` runs full SMTP sessions against a fake backend, including STARTTLS (fixture cert in `tests/fixtures/`) and cert reload.

## Sponsor

This project is sponsored by [tempy.email](https://tempy.email).

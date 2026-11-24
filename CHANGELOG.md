# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- Redis errors now answer `451 4.3.0` (retry later) instead of `550 User unknown`, so a Redis outage no longer bounces mail permanently
- Oversized messages are read through to the end before `552` is sent; previously the rest of the body was parsed as SMTP commands
- DATA memory use is bounded by `MAX_MESSAGE_SIZE` even for input without newlines
- `SIZE` in the EHLO reply now reflects `MAX_MESSAGE_SIZE`; `MAIL FROM ... SIZE=` over the limit is rejected up front
- Backend replies: multi-line responses are parsed correctly, a backend 5xx is reported to the sender as permanent, a 4xx recipient rejection defers the whole message, and the end-of-DATA reply is checked
- Command order enforced (HELO/EHLO before MAIL, MAIL before RCPT/DATA, no nested MAIL); `FROM:`/`TO:` keywords and paths validated; null sender `<>` relayed correctly
- Over-long command lines get `500 5.5.2 Line too long` and the session continues
- Over-long command words are no longer truncated into valid commands (e.g. `STARTTLSX`)
- HELO gets a plain single-line reply; EHLO/HELO without a hostname gets `501`

### Added

- `Received:` header with the real client IP on every relayed message
- TLS certificate hot reload (SIGHUP and `TLS_RELOAD_INTERVAL`); failed reloads keep the old cert
- Graceful shutdown on SIGTERM with `SHUTDOWN_TIMEOUT`
- `IDLE_TIMEOUT`, `RELAY_TIMEOUT`, `REDIS_TIMEOUT_MS`, `MAX_CONCURRENT_PER_IP`
- `421` reply when the connection limit is reached (was: left waiting in the accept queue)
- Per-IP limits group IPv6 by /64
- `deferred` metric and `[MAIL-DEFERRED]` log tag
- End-to-end session tests, including STARTTLS
- CI: `cargo audit` (also weekly), `--locked` builds, Actions pinned by SHA, Dependabot; Docker images publish only after CI passes, with SBOM and provenance

### Changed

- DATA is dot-unstuffed and normalized to CRLF on receipt and re-stuffed on relay; bare CR is rejected (SMTP smuggling hardening)
- Subdomain wildcard matches at any depth
- Dependencies: redis 1.8, OpenTelemetry 0.33, `rustls-pemfile` (unmaintained) replaced by `rustls-pki-types`; lockfile refreshed, clearing RustSec advisories in aws-lc-sys, rustls and h2
- MSRV is now 1.88

## [0.1.0] - 2026-02-16

### Added

- Initial release as Burngate (formerly smtp-gateway)
- SMTP protocol handler (EHLO, HELO, MAIL FROM, RCPT TO, DATA, RSET, NOOP, QUIT, VRFY)
- Redis-based mailbox existence check at RCPT TO stage
- Two-tier lookup: active mailbox key (`mb:{address}`) with permanent fallback (`addresses` set)
- SMTP relay to backend server for accepted messages
- STARTTLS support via rustls
- Multi-domain support with subdomain wildcard matching
- Structured JSON logging with tracing
- Metrics reporting (accepted/rejected/connections/errors) every 60 seconds
- Configurable via environment variables
- Docker image
- systemd unit file with security hardening

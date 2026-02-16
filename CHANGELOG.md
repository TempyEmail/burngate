# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

# Security Policy

## Supported versions

| Version | Supported |
|---------|-----------|
| 0.1.x   | Yes       |

## Reporting a vulnerability

If you discover a security vulnerability in Burngate, please report it responsibly.

**Do not open a public issue.**

Instead, email: **security@tempy.email**

Include:
- Description of the vulnerability
- Steps to reproduce
- Potential impact
- Suggested fix (if any)

## Response timeline

- **Acknowledgment**: Within 48 hours
- **Initial assessment**: Within 1 week
- **Fix and release**: As soon as possible, depending on severity

## Scope

This project handles SMTP traffic on port 25. Security issues we care about include:

- Buffer overflow or memory safety issues
- SMTP protocol injection or smuggling
- Redis command injection
- Denial of service (resource exhaustion, connection flooding)
- TLS configuration weaknesses
- Open relay vulnerabilities (accepting/relaying mail for unauthorized domains)

## Out of scope

- Spam content filtering (this project only checks recipient existence)
- Issues in upstream dependencies (report those upstream, but let us know too)
- Social engineering

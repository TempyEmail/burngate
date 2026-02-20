<img src="logo.svg" alt="Burngate" width="100">

# Burngate

**Lightweight SMTP gateway for disposable email routing. Rust + Redis.**

[![CI](https://github.com/TempyEmail/burngate/actions/workflows/ci.yml/badge.svg)](https://github.com/TempyEmail/burngate/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Docker](https://github.com/TempyEmail/burngate/actions/workflows/docker.yml/badge.svg)](https://github.com/TempyEmail/burngate/actions/workflows/docker.yml)

Burngate sits in front of your mail server on port 25 and rejects mail for nonexistent recipients **at the `RCPT TO` stage** -- before the message body is ever transmitted.

Sponsored by [tempy.email](https://tempy.email) -- free disposable email for everyone.

```
Internet --> :25 burngate --> Redis lookup
                                |-- not found --> 550 reject (no body transferred)
                                '-- found --> accept DATA --> relay to backend :2525
```

## The problem

If you run an SMTP server that accepts mail for dynamically-created addresses (disposable email, SaaS tenants, catch-all domains), you're probably processing a lot of spam for addresses that don't exist. The typical flow is:

1. Spammer connects to port 25
2. Your server accepts the connection, receives the full email body
3. Your application parses the MIME message, checks the database... address doesn't exist
4. Email is discarded. But you already spent CPU, memory, and bandwidth on it.

Most of these emails target addresses that were never created. The waste happens because the existence check comes **after** the email body has been received.

## The solution

SMTP has a natural checkpoint: the `RCPT TO` command. The sender announces the recipient **before** sending the message body (`DATA`). Burngate intercepts at that stage:

1. Sender connects, sends `EHLO`, `MAIL FROM` -- gateway accepts normally
2. Sender sends `RCPT TO:<someone@yourdomain.com>` -- gateway checks Redis
3. **Not in Redis** -- respond `550 5.1.1 User unknown`. Done. No body transferred
4. **In Redis** -- accept, receive `DATA`, relay to your backend

Spam to nonexistent addresses never transmits a single byte of body content.

## Features

- **Pre-DATA filtering** -- rejects unknown recipients before body transfer
- **Redis-backed** -- sub-millisecond mailbox existence checks
- **Configurable Redis schema** -- bring your own key pattern (`user:{address}`, `mailbox:{address}:active`, whatever you use)
- **Multi-domain** -- configure multiple accepted domains, with wildcard subdomain support
- **STARTTLS** -- optional TLS upgrade using rustls (configurable cert/key paths)
- **SMTP relay** -- forwards accepted mail to your backend via standard SMTP
- **Structured logging** -- JSON logs via `tracing`, configurable with `RUST_LOG`
- **Metrics** -- accepted/rejected/connections/errors logged every 60 seconds
- **Tiny footprint** -- single static binary, ~5MB, minimal memory usage
- **Async** -- built on tokio, handles thousands of concurrent connections
- **Docker-ready** -- works as a sidecar forwarding to another container

## Quick start

### Docker (recommended)

```bash
docker run -d \
  --name burngate \
  -p 25:25 \
  -e ACCEPTED_DOMAINS=example.com,example.org \
  -e REDIS_HOST=your-redis-host \
  -e BACKEND_SMTP=your-backend:2525 \
  -e REDIS_KEY_PATTERN='mailbox:{address}' \
  tempyemail/burngate:latest
```

### Docker Compose (gateway + backend)

```yaml
services:
  burngate:
    image: tempyemail/burngate:latest
    ports:
      - "25:25"
    environment:
      - ACCEPTED_DOMAINS=example.com,example.org
      - REDIS_HOST=redis
      - BACKEND_SMTP=mailserver:2525
      - REDIS_KEY_PATTERN=mailbox:{address}
      - REDIS_CHECK_MODE=key
    depends_on:
      - redis
      - mailserver

  mailserver:
    image: your-mail-server:latest
    # No port 25 exposed -- only reachable via gateway
    expose:
      - "2525"

  redis:
    image: redis:7-alpine
```

The gateway resolves `mailserver` and `redis` via Docker's internal DNS. No ports need to be published for inter-container communication -- `expose` is enough.

### From source

```bash
git clone https://github.com/TempyEmail/burngate.git
cd burngate
cargo build --release
# Binary at target/release/burngate

ACCEPTED_DOMAINS=example.com REDIS_HOST=127.0.0.1 BACKEND_SMTP=127.0.0.1:2525 \
  ./target/release/burngate
```

### Local testing

The included `docker-compose.yml` starts Burngate, Redis, and a mock backend SMTP server.

**1. Start everything**

```bash
docker compose up --build
```

**2. Seed a test mailbox in Redis**

```bash
docker compose exec redis redis-cli SET mb:test@example.com 1
```

**3. Send test emails with [swaks](https://www.jetmore.org/john/code/swaks/)**

```bash
# Should ACCEPT -- mailbox exists in Redis, message relayed to backend
swaks --to test@example.com --from sender@other.com --server localhost:25

# Should REJECT 550 -- mailbox not found
swaks --to nobody@example.com --from sender@other.com --server localhost:25

# Should REJECT 550 -- domain not accepted
swaks --to user@evil.com --from sender@other.com --server localhost:25
```

Watch the Burngate output for `[RCPT-ACCEPTED]`, `[MAIL-REJECTED]`, and `[MAIL-RELAYED]` tags.

> **Note:** The `docker-compose.yml` includes [smtp4dev](https://github.com/rnwood/smtp4dev) as the mock backend on port 2525. Open [http://localhost:3000](http://localhost:3000) to see relayed messages in the web UI. Replace it with your real mail server in production.

**Cleanup:**

```bash
docker compose down
```

## Configuration

All configuration is via environment variables.

### Network

| Variable | Default | Description |
|---|---|---|
| `LISTEN_ADDR` | `0.0.0.0:25` | Address and port to listen on |
| `BACKEND_SMTP` | `127.0.0.1:2525` | Backend SMTP server to relay accepted mail to |
| `ACCEPTED_DOMAINS` | **required** | Comma-separated list of accepted domains |
| `SERVER_NAME` | `burngate` | Hostname used in SMTP banner and EHLO response |
| `MAX_MESSAGE_SIZE` | `10485760` (10MB) | Maximum message size in bytes |
| `CONNECTION_TIMEOUT` | `300` | Connection timeout in seconds |
| `METRICS_INTERVAL` | `60` | Metrics log interval in seconds. Set to `0` to disable |

### Redis

| Variable | Default | Description |
|---|---|---|
| `REDIS_URL` | -- | Full Redis URL (overrides individual vars below) |
| `REDIS_HOST` | `127.0.0.1` | Redis hostname |
| `REDIS_PORT` | `6379` | Redis port |
| `REDIS_USERNAME` | -- | Redis username (optional) |
| `REDIS_PASSWORD` | -- | Redis password (optional) |
| `REDIS_KEY_PATTERN` | `mb:{address}` | Key pattern for mailbox lookup. `{address}` is replaced with the lowercased recipient |
| `REDIS_SET_NAME` | `addresses` | Redis SET name for fallback check. Set to empty to disable |
| `REDIS_CHECK_MODE` | `both` | Which checks to run: `key` (EXISTS only), `set` (SISMEMBER only), `both` (key first, then set fallback) |

### TLS

| Variable | Default | Description |
|---|---|---|
| `TLS_CERT_PATH` | -- | Path to PEM certificate for STARTTLS |
| `TLS_KEY_PATH` | -- | Path to PEM private key for STARTTLS |

### Logging

| Variable | Default | Description |
|---|---|---|
| `RUST_LOG` | `info` | Log level: `trace`, `debug`, `info`, `warn`, `error` |

### Observability (OpenTelemetry)

| Variable | Default | Description |
|---|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | -- | OTLP gRPC endpoint. When set, traces are exported via OTLP. Example: `http://localhost:4317`. Unset = OTel disabled, zero overhead |
| `OTEL_SERVICE_NAME` | `burngate` | Service name reported in traces |

When `OTEL_EXPORTER_OTLP_ENDPOINT` is set, every SMTP session becomes a root span (`smtp.session`) with each relay as a child span (`smtp.relay`). A W3C `traceparent` header is injected into the outgoing email so downstream services can continue the trace.

## Redis key format

The gateway checks Redis for each recipient address (lowercased). The check behavior depends on `REDIS_CHECK_MODE`:

**`key` mode** -- runs `EXISTS <key>` using `REDIS_KEY_PATTERN`:
```
REDIS_KEY_PATTERN=mb:{address}
# For user@example.com, checks: EXISTS mb:user@example.com
```

**`set` mode** -- runs `SISMEMBER <set> <address>` using `REDIS_SET_NAME`:
```
REDIS_SET_NAME=active_mailboxes
# For user@example.com, checks: SISMEMBER active_mailboxes user@example.com
```

**`both` mode** (default) -- tries key first, falls back to set. Useful when the key has a TTL and the set is permanent.

### Examples for different applications

```bash
# Rails app with Redis-backed sessions
REDIS_KEY_PATTERN="user:mailbox:{address}" REDIS_CHECK_MODE=key

# Simple set of active addresses
REDIS_SET_NAME="active_emails" REDIS_CHECK_MODE=set

# Disposable email service (key with TTL + permanent set)
REDIS_KEY_PATTERN="mb:{address}" REDIS_SET_NAME="addresses" REDIS_CHECK_MODE=both
```

## How it works

```
Client                    Gateway                     Redis           Backend
  |                         |                           |                |
  |---EHLO---------------->|                           |                |
  |<--250 OK---------------|                           |                |
  |---MAIL FROM:<...>----->|                           |                |
  |<--250 OK---------------|                           |                |
  |---RCPT TO:<user@dom>-->|                           |                |
  |                         |---EXISTS mb:user@dom---->|                |
  |                         |<--0 (not found)----------|                |
  |                         |---SISMEMBER addresses--->|                |
  |                         |<--0 (not found)----------|                |
  |<--550 User unknown------|                           |                |
  |                         |                           |                |
  |---RCPT TO:<real@dom>-->|                           |                |
  |                         |---EXISTS mb:real@dom---->|                |
  |                         |<--1 (found)--------------|                |
  |<--250 OK---------------|                           |                |
  |---DATA---------------->|                           |                |
  |---<message body>------->|                           |                |
  |---.----------------------|                           |                |
  |                         |----------SMTP relay--------------------->|
  |                         |<---------250 OK--------------------------|
  |<--250 OK---------------|                           |                |
```

## Deployment

### Docker (container-to-container)

Burngate works well as a sidecar in Docker Compose or Kubernetes. Set `BACKEND_SMTP` to the service name of your mail server container:

```yaml
environment:
  - BACKEND_SMTP=mailserver:2525  # Docker DNS resolves this
```

Only the gateway needs port 25 published. Your backend mail server stays internal.

### systemd

A sample unit file is included at `burngate.service`:

```bash
sudo cp burngate.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now burngate
```

The service runs as a dedicated `burngate` user with `CAP_NET_BIND_SERVICE` to bind port 25 without root.

### Backend changes

Your existing SMTP server should move to an internal port (e.g., `2525`) and bind only to `127.0.0.1`. The gateway handles all external connections on port 25 and relays accepted mail to your backend.

## Distributed tracing (OpenTelemetry)

Burngate supports [OpenTelemetry](https://opentelemetry.io/) traces via OTLP. When enabled, each inbound SMTP connection produces a root span and the relay step produces a child span. A W3C `traceparent` header is injected into every relayed email so the receiving mail server can attach its own processing spans to the same trace.

```
smtp.session (peer=1.2.3.4:51234)
  └── smtp.relay (sender=..., recipients=[...], size=1234)
       [traceparent injected into email headers]
            ↓
       Downstream mail server reads traceparent → continues trace
```

### Setup with .NET Aspire

Point burngate at the Aspire dashboard OTLP endpoint:

```yaml
environment:
  OTEL_EXPORTER_OTLP_ENDPOINT: http://localhost:15901
  OTEL_SERVICE_NAME: burngate
```

Traces appear in the Aspire dashboard alongside your .NET services. To stitch traces end-to-end (burngate → your mail processor → downstream APIs), extract the `traceparent` MIME header from the received message and pass it as the parent context when starting your processing activity.

### Setup with any OTLP backend

```yaml
environment:
  OTEL_EXPORTER_OTLP_ENDPOINT: http://otel-collector:4317
  OTEL_SERVICE_NAME: burngate
```

Compatible with Jaeger, Grafana Tempo, Honeycomb, Datadog, and any OTLP-compatible backend.

### Disabling OTel

Do not set `OTEL_EXPORTER_OTLP_ENDPOINT`. The OTel layer is not initialized and there is no runtime overhead.

## Monitoring

The gateway logs metrics every 60 seconds in structured JSON:

```json
{
  "level": "INFO",
  "message": "[METRICS]",
  "accepted": 1523,
  "rejected": 48291,
  "connections": 49814,
  "relay_errors": 0
}
```

Key log tags for filtering:
- `[RCPT-ACCEPTED]` -- mailbox verified, accepting mail
- `[MAIL-REJECTED]` -- mailbox not found or unknown domain
- `[MAIL-RELAYED]` -- message forwarded to backend
- `[RELAY-ERROR]` -- backend relay failed
- `[METRICS]` -- periodic counters

### Watching metrics live

Stream metrics from a running container:

```bash
# Follow only [METRICS] lines
docker compose logs -f burngate | grep METRICS

# Or use watch to poll the latest metrics line every second
watch -n 1 'docker compose logs burngate 2>&1 | grep METRICS | tail -1 | sed "s/^[^{]*//" | jq'
```

Set `METRICS_INTERVAL` to control how often metrics are emitted (default 60s, set to `0` to disable):

```yaml
environment:
  - METRICS_INTERVAL=300   # every 5 minutes (production)
  - METRICS_INTERVAL=10    # every 10 seconds (debugging)
  - METRICS_INTERVAL=0     # disabled
```

## Alternatives

Burngate is purpose-built for a single task: check Redis at `RCPT TO` and reject unknown recipients. If you need more, here are other options:

| | Burngate | [Haraka](https://github.com/haraka/Haraka) | [Postfix](http://www.postfix.org/) | [OpenSMTPD](https://www.opensmtpd.org/) |
|---|---|---|---|---|
| **Purpose** | RCPT TO filter + relay | Full MTA + plugin framework | Full MTA | Full MTA |
| **Language** | Rust | Node.js | C | C |
| **Runtime** | Static binary | Node.js required | OS package | OS package |
| **Memory** | ~2-5MB | ~50-100MB+ | ~10-30MB | ~5-15MB |
| **Recipient check** | Built-in (Redis) | Plugin (`rcpt_to` hook) | `check_recipient_access` | Table lookups |
| **SPF/DKIM/DNSBL** | No | Yes (plugins) | Yes (milter) | Via filters |
| **Outbound queue** | No (relay only) | Yes | Yes | Yes |
| **Config** | 12 env vars | Plugin config files | main.cf + master.cf | smtpd.conf |
| **Best for** | Single-check pre-filter | Extensible filtering MTA | General-purpose MTA | Simple, secure MTA |

**When to use Burngate:**
- You need exactly one thing: "does this address exist in Redis?"
- You want the smallest possible service in front of your existing mail server
- You run dynamic/disposable addresses where the address list lives in Redis
- You want a Docker sidecar that just filters and relays

**When to use something else:**
- You need SPF, DKIM, DNSBL, SpamAssassin, rate limiting -- use Haraka or Postfix with milters
- You need a complete mail server with queuing and delivery -- use Postfix or OpenSMTPD
- You want a plugin ecosystem without compiling anything -- use Haraka

Burngate is designed to complement these tools, not replace them. You can run it in front of Postfix, Haraka, or any other SMTP server.

## Sponsor

This project is sponsored by ![tempy.email](https://tempy.email/favicon-32x32.png) [tempy.email](https://tempy.email) -- a free, privacy-first disposable email service. Built to handle millions of inbound emails, the gateway was born out of the need to efficiently filter spam before it reaches the application layer.

Learn more at [tempy.email/burngate](https://tempy.email/burngate).

## License

MIT -- see [LICENSE](LICENSE).

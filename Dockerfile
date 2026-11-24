# Base images are pinned by digest; Dependabot bumps them.
FROM rust:1.93-slim@sha256:c0a38f5662afdb298898da1d70b909af4bda4e0acff2dc52aea6360a9b9c6956 AS builder

WORKDIR /app

# Cache dependencies by building a dummy project first
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs
RUN cargo build --locked --release && rm -rf src target/release/deps/burngate* target/release/deps/libburngate* target/release/.fingerprint/burngate*

# Build the real project
COPY src/ src/
RUN cargo build --locked --release

# Runtime stage -- minimal image
FROM debian:trixie-slim@sha256:8aa5a7bf5a1955bddbe7d74fcdd0b91ab30e74362a8b73af672b0481b35b44a0

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --system --no-create-home burngate

COPY --from=builder /app/target/release/burngate /usr/local/bin/burngate

USER burngate

ENV LISTEN_ADDR=0.0.0.0:25
ENV BACKEND_SMTP=127.0.0.1:2525
ENV REDIS_HOST=127.0.0.1
ENV REDIS_PORT=6379
ENV REDIS_KEY_PATTERN=mb:{address}
ENV REDIS_SET_NAME=addresses
ENV REDIS_CHECK_MODE=both
ENV RUST_LOG=info
# ACCEPTED_DOMAINS is required — set it at runtime
# e.g. docker run -e ACCEPTED_DOMAINS=example.com,mail.example.com ...

EXPOSE 25

ENTRYPOINT ["burngate"]

FROM rust:1.93-slim AS builder

WORKDIR /app

# Cache dependencies by building a dummy project first
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs
RUN cargo build --release && rm -rf src target/release/deps/burngate*

# Build the real project
COPY src/ src/
RUN cargo build --release

# Runtime stage -- minimal image
FROM debian:trixie-slim

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

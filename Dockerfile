# ── Stage 1: build ──────────────────────────────────────────────────────────
FROM rust:1-slim-bookworm AS builder

WORKDIR /app

# Cache dependency compilation separately from source changes.
# Copy manifests first and build a dummy binary so the dependency layer is
# cached as long as Cargo.toml / Cargo.lock don't change.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release \
    && rm -rf src

# Now copy real source and rebuild only the app crate.
COPY src ./src
RUN touch src/main.rs && cargo build --release

# ── Stage 2: minimal runtime image ──────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/nerd-watch /usr/local/bin/nerd-watch

# nerd-watch needs the Docker socket; mount it at runtime:
#   -v /var/run/docker.sock:/var/run/docker.sock
ENTRYPOINT ["/usr/local/bin/nerd-watch"]

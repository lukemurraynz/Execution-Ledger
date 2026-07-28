FROM mcr.microsoft.com/devcontainers/rust:2-bookworm

# libpq-dev: sqlx's Postgres driver is pure-Rust (no libpq needed to build),
# but psql/pg_isready are useful for manual verification against the
# devcontainer's Postgres service. pkg-config/libssl-dev cover native-tls
# transitive deps pulled in by drasi-reaction-http's HTTP client.
RUN apt-get update && apt-get install -y --no-install-recommends \
    postgresql-client \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

RUN rustup component add rustfmt clippy rust-analyzer

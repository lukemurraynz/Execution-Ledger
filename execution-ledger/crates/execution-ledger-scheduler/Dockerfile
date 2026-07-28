FROM rust:1.97-bookworm@sha256:77fac8b98f9f46062bb680b6d25d5bcaabfc400143952ebc572e924bcbedc3fa AS builder
WORKDIR /workspace

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY vendor ./vendor
RUN cargo build --release -p execution-ledger-scheduler

FROM debian:bookworm-slim@sha256:7b140f374b289a7c2befc338f42ebe6441b7ea838a042bbd5acbfca6ec875818
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && groupadd --gid 10001 appgroup \
    && useradd --uid 10001 --gid appgroup --create-home --shell /usr/sbin/nologin appuser \
    && rm -rf /var/lib/apt/lists/*

LABEL org.opencontainers.image.title="execution-ledger-scheduler" \
      org.opencontainers.image.description="Execution Ledger scheduler service" \
      org.opencontainers.image.licenses="UNLICENSED"

WORKDIR /app
COPY --chown=10001:10001 --from=builder /workspace/target/release/execution-ledger-scheduler /usr/local/bin/execution-ledger-scheduler

ENV RUST_LOG=info
STOPSIGNAL SIGTERM

USER 10001:10001
ENTRYPOINT ["execution-ledger-scheduler"]

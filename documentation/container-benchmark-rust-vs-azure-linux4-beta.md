# Rust Container Benchmark: Standard Runtime vs Azure Linux 4 (beta)

> **Blog idea.** This is reference material for a future micro-blog or performance post, not living project documentation. Publish, then delete from the repo.

## Scope

This benchmark compares the `execution-ledger-api` container using:

1. Standard runtime base (current codebase): `debian:bookworm-slim`
2. Experimental runtime base: `mcr.microsoft.com/azurelinux-beta/base/core:4.0`

Build platform: Azure Container Registry Tasks (`az acr build`), same source and same Rust builder stage.

## Results

| Variant | Image tag | Build time (s) | Image size (bytes) | Image size (MiB) |
|---|---|---:|---:|---:|
| Standard (current) | `benchmark/execution-ledger-api:rust-bookworm` | 367.61 | 37804305 | 36.05 |
| Azure Linux 4 beta | `benchmark/execution-ledger-api:azurelinux4-beta` | 367.41 | 52864245 | 50.42 |

## Observations

- Build time was effectively the same.
- Azure Linux 4 beta image was materially larger for this workload.
- For this codebase, the current standard runtime image remains the better default unless there is a compliance/platform requirement to use Azure Linux.

## Codebase state

- The production Dockerfiles remain on the standard Rust + Debian runtime pattern.
- The Azure Linux 4 test Dockerfile was used only for benchmarking and is not required for normal deployment.

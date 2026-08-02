# Changelog

All notable changes to Execution Ledger will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Initial public release of Execution Ledger.
- Durable job orchestration with step-level idempotency, replay, skip, and compensate controls.
- Tenant-scoped API with Microsoft Entra ID authentication and authorization.
- Connector packs for REST, queue, Azure Blob, SFTP, FTP, ERP, PGP, MS Graph Notify, Azure Compute, Azure SQL, transform, blob archive, HTTP download, and batch window integrations.
- Drasi WAL reader for change-driven orchestration from PostgreSQL.
- Cron/timezone scheduling with the scheduler service.
- KEDA-driven worker autoscaling on Service Bus queue depth.
- MCP endpoint for AI copilot integration with scope-gated tool discovery.
- Tauri desktop operator console for job management and recovery workflows.
- Operational proof harness with chaos testing, DR integrity, and capacity/cost gate validation.
- Managed identity-first production posture with no connection strings or access keys.
- Bicep infrastructure and `azd` deployment support.

[Unreleased]: https://github.com/lukemurraynz/Execution-Ledger/compare/v0.1.0...HEAD

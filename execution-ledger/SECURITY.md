# Security Policy

## Reporting a vulnerability

Please report security vulnerabilities privately rather than opening a public GitHub issue. Use [GitHub's private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing/privately-reporting-a-security-vulnerability) for this repository (the "Security" tab → "Report a vulnerability"), if enabled. If it isn't, open an issue asking for a private contact channel rather than describing the vulnerability publicly.

Include, if known:

- A description of the vulnerability and its potential impact.
- Steps to reproduce, including a minimal example if possible.
- The affected version/commit.
- Any suggested mitigation.

We aim to acknowledge reports within a few business days. Coordinated disclosure is preferred: please give us a reasonable window to investigate and ship a fix before any public disclosure.

## Scope

This project handles Azure credentials and secrets (Entra ID tokens, managed identity, Key Vault references, Service Bus and PostgreSQL connection details) as part of its normal operation. Vulnerability categories of particular interest:

- Authentication or authorization bypass in the API (Entra JWT validation, tenant scoping, MCP scope enforcement).
- Cross-tenant data leakage — any path where one tenant can read, replay, skip, or compensate another tenant's runs.
- Secret exposure — credentials or tokens appearing in logs, error responses, or step outputs.
- Injection in connector-step execution (path traversal in the `sftp`/`azure_blob` connectors, SSRF via the `rest`/`erp` connectors bypassing the header denylist, unsafe deserialization).
- Privilege escalation through connector-pack job templates or the MCP tool surface.

## Not in scope

- Vulnerabilities requiring physical access to a deployed environment.
- Issues in third-party dependencies — please report those to the upstream project directly, though we're glad to hear about them too so we can track an upgrade.
- Denial-of-service reports without a concrete resource-exhaustion mechanism.

## Supported versions

This project does not yet have a formal release/support cadence. Security fixes land on `main`; there is no maintained older branch at this time.

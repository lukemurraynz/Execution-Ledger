# PGP Crypto Pack

## Purpose

Encrypt-and-sign or decrypt-and-verify a file payload using OpenPGP, as a connector step rather than a standalone encrypt/decrypt microservice called out-of-band from an orchestrator — the crypto operation is a step in the same run, with the same idempotency/replay/audit guarantees as every other step.

## Use this pack to

1. Decrypt and verify inbound partner files (e.g. bank statements, payment files) delivered over SFTP before they reach downstream storage.
2. Encrypt and sign outbound files before handing them to a delivery step.
3. Validate that a partner's signing key rotation didn't break verification, by replaying the `decrypt-file` step in isolation against a captured payload.

## Prerequisites

- An OpenPGP key pair for this integration (your private key + the partner's public key), each ASCII-armored and then base64-encoded once more for storage as a secret value.
- All three PGP secrets stored as Key Vault references in App Configuration — this pack does not support raw key material in plain environment variables outside local/dev.
- SFTP credentials configured if this pack is chained with the `sftp` connector (see `sftp-batch-pack` for the SFTP configuration contract).

## Configuration

| Variable | Required | Description |
|---|---|---|
| `EXECUTION_LEDGER_PGP_PARTNER_PUBLIC_KEY` | Yes | Base64-encoded ASCII-armored partner public key (encrypt recipient / decrypt signature check) |
| `EXECUTION_LEDGER_PGP_PRIVATE_KEY` | Yes | Base64-encoded ASCII-armored own private key (encrypt signer / decrypt recipient) |
| `EXECUTION_LEDGER_PGP_PRIVATE_KEY_PASSPHRASE` | Yes, via Key Vault | Passphrase unlocking the private key |

Each can also be set per-step in the job's `config` block (useful for testing against a throwaway key pair without touching global config).

## How the pgp connector composes with other steps

`pgp` does not read or write blob storage itself — it consumes and produces the same `{ "content": "<base64>", "contentEncoding": "base64" }` shape the `sftp` connector's `download` action already produces and the `azure_blob`/`sftp upload` connectors already consume. This keeps the crypto step a pure transform:

- **Inbound**: `sftp` (download) → `pgp` (decrypt) → `azure_blob` (upload).
- **Outbound**: `pgp` (encrypt) → `sftp` (upload), or `pgp` (encrypt) → `azure_blob` (upload).

Both chains work the same way: leave the consuming step's `data`/`input` empty and it falls back to the immediately preceding step's base64 output. `sftp upload`'s fallback only kicks in when its own `input` is empty/null — if a job author puts real data in `input`, that still uploads verbatim (the original, unchanged behavior), so existing job templates that don't chain are unaffected.

**Each step only sees the output of the one step directly before it — there is no fan-out.** If you need both an audit copy in blob storage and delivery over SFTP from the same encrypted file, don't insert a second consumer step after `encrypt-file` expecting it to also read `encrypt-file`'s output (it will see the step that ran between them instead). Use two separate steps back-to-back that each read from `encrypt-file` isn't possible in one run either, since `ctx.upstream_output` always reflects the immediately preceding step — run a second job, or store the encrypted bytes once and have a downstream consumer read that stored copy for delivery.

## Signature verification failure mode (read this before debugging a decrypt failure)

`decrypt` fails closed in two distinct ways, both surfaced as `DependencyFailed` with different `dependency` tags in the error:

- `dependency: "pgp"` — key parsing, malformed ciphertext, or a generic decryption failure. Usually a config or file-corruption problem.
- `dependency: "pgp-signature-verification"` — decryption succeeded (the file readable with your private key) but the signature does not verify against `partnerPublicKey`. **The step still fails and no content is emitted** — a downstream `azure_blob` step is never reached, so nothing unverified is ever persisted. This usually means the partner rotated their signing key and `EXECUTION_LEDGER_PGP_PARTNER_PUBLIC_KEY` is stale, not that the file itself is corrupt.

## Run it

```bash
cargo run -p execution-ledger-cli -- job-launch <tenant-id> pgp-crypto.decrypt-inbound-file '{"steps":[{"id":"health-check","connector":"platform_health"},{"id":"download-file","connector":"sftp","action":"download","config":{"remotePath":"/inbox/statement.pgp"}},{"id":"decrypt-file","connector":"pgp","action":"decrypt","config":{"partnerPublicKey":"<b64>","privateKey":"<b64>","privateKeyPassphrase":"<secret>"}},{"id":"store-decrypted-file","connector":"azure_blob","config":{"container":"inbound-decrypted","blobName":"statement.txt"}}]}'
```

## Failure modes

| Symptom | Likely cause | Fix |
|---|---|---|
| `decrypt-file` fails with `InvalidConfiguration` naming `partnerPublicKey`/`privateKey`/`privateKeyPassphrase` | Secret not resolved from Key Vault, or missing from step config | Check the App Configuration key exists and the Key Vault reference resolves |
| `decrypt-file` fails with `InvalidConfiguration` naming "no base64 content" | Upstream step didn't produce `{content, contentEncoding: "base64"}`, or this is the first step in the run | Ensure the step before `decrypt-file` is `sftp` (download) or provide `input.content` directly |
| `decrypt-file` fails with `DependencyFailed { dependency: "pgp-signature-verification" }` | Partner rotated their signing key, or the wrong public key is configured | Confirm the current partner public key with the partner and update the Key Vault secret; do not treat this as a transient failure to retry blindly |
| `decrypt-file` fails with `DependencyFailed { dependency: "pgp" }` | Wrong private key, wrong passphrase, or corrupt/truncated source file | Re-download the source file and confirm the private key matches what the partner encrypted to |
| `upload-file` uploads a JSON-wrapped payload instead of the encrypted file | The `sftp` upload step's own `data`/`input` has an explicit non-empty value, which takes priority over `encrypt-file`'s output (same rule `azure_blob` follows) | Remove `data`/`input` from the `upload-file` step so it falls back to reading `encrypt-file`'s output |
| `upload-file` uploads the wrong step's output | Another step was inserted between `encrypt-file` and `upload-file` | `ctx.upstream_output` always reflects only the immediately preceding step — there is no fan-out to a step two positions back; keep `upload-file` directly after `encrypt-file` |

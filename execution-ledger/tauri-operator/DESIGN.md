# Tauri operator design baseline

This app ships a tokenized visual baseline and shared section primitives for a production-ready operator surface.

## Tokens

- Canonical design tokens live in `src/styles/tokens.css`.
- `App.css` consumes those tokens for colors, spacing, radii, and typography.
- `tailwind.config.ts` mirrors the same token names under `theme.extend` so utility styling can be added without introducing a second token system.

## Shared UI primitives

- `src/components/Pill.tsx`: normalized status badge rendering.
- `src/components/SectionHead.tsx`: consistent section eyebrow, title, and summary note layout.

## UX invariants

- Every destructive operation keeps a user confirmation prompt.
- Recovery inputs enforce bounded integer limits before API calls.
- Session secrets stay in-process memory only and are not persisted to local storage.

## Security baseline

- Tauri CSP is explicitly set in `src-tauri/tauri.conf.json` and must not be set back to `null`.
- APIM gateway calls can run via bearer token and can also attach APIM HMAC client headers when a gateway secret is present.

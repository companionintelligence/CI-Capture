# portal_discover.rs

## Purpose
Best-effort lookup of the user's CI-Server (`ci-memory`) public URL from
CI-Portal after OIDC browser sign-in, so onboarding can autofill
`server.portal_url`.

## Components

### `portal_origin_from_issuer(issuer)`
- **Does**: Strip a trailing `/api/auth` from the OIDC issuer and return the
  portal origin (`https://host`).

### `discover_ci_server_url(issuer, bearer)`
- **Does**: `GET {origin}/api/devices` (optional Bearer), scan device apps for
  slug/name matching `ci-memory` / Companion Memory, return the first URL.
- **Rationale**: Soft-fail — Portal may require a cookie session rather than an
  OIDC access token; callers leave the field for manual entry when discovery
  returns `None` or errors.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| `onboarding_ui.rs` | Soft-fail discovery after login | Changing return semantics to hard-fail |

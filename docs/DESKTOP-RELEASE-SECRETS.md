# Companion Capture — desktop release secrets

Secrets for [`.github/workflows/release.yml`](../.github/workflows/release.yml) (**Desktop Release**). They are **GitHub Environment secrets**, not local `.env` files — CI-Capture has no stack env file like CI-Hub.

## 1. Create GitHub Environments

In **companionintelligence/CI-Capture** → **Settings** → **Environments**, create:

| Environment | Used for |
|-------------|----------|
| `production` | Tag pushes (`v*`) and production manual releases |
| `dev` | Prerelease manual runs (`v0.1.0-dev.1`, etc.) |

Add the same secret **names** to **both** environments. Values can differ (e.g. dev uses test certs) or be identical while bootstrapping.

## 2. Secret inventory

### Copy from CI-Hub (same values)

These are shared with CI-Hub **GitHub Environment** secrets (`production` / `dev`). Copy from your vault entry **CI-Hub / PROD** (or DEV) → GitHub Actions.

| Secret | Purpose |
|--------|---------|
| `APPLE_CERTIFICATE` | Base64-encoded `.p12` Developer ID Application certificate |
| `APPLE_CERTIFICATE_PASSWORD` | Password for the `.p12` |
| `APPLE_SIGNING_IDENTITY` | Optional but recommended — exact keychain identity, e.g. `Developer ID Application: LifeScope Inc. (TEAMID)` |
| `APPLE_ID` | Apple developer account email for notarization |
| `APPLE_PASSWORD` | App-specific password for notarization |
| `APPLE_TEAM_ID` | Apple Developer Team ID |
| `AZURE_TENANT_ID` | Microsoft Entra tenant (directory) ID |
| `AZURE_CLIENT_ID` | Entra app registration client ID (used by `azure/login` OIDC) |
| `AZURE_SUBSCRIPTION_ID` | Azure subscription containing the Artifact Signing account |
| `AZURE_SIGNING_ENDPOINT` | Regional endpoint, e.g. `https://eus.codesigning.azure.net/` |
| `AZURE_SIGNING_ACCOUNT` | Artifact Signing account name |
| `AZURE_CERTIFICATE_PROFILE` | Certificate profile name |

### CI-Capture only (not in CI-Hub desktop release)

| Secret | Purpose | Status |
|--------|---------|--------|
| `LINUX_GPG_PRIVATE_KEY` | ASCII-armored or base64-encoded GPG **private** key for detached `.asc` signatures on release assets | **Required for production Linux** |
| `LINUX_GPG_PASSPHRASE` | Passphrase for that key; leave empty if the key has none | Optional |
| `SENTRY_DSN` | Ingest DSN for the CI-Capture desktop Sentry project, baked into the binary at build time so users who opt in can send scrubbed panic reports | Optional |

Production Linux releases **fail** without `LINUX_GPG_PRIVATE_KEY`. macOS and Windows require their signing secrets when building those platforms in production.

#### `SENTRY_DSN` and what happens without it

The **Build installers** step passes `SENTRY_DSN` to `cargo packager` as an environment variable; [`src/telemetry.rs`](../src/telemetry.rs) reads it at compile time with `option_env!`. It is deliberately **not** a `.env` value and is never set on a developer machine — a developer's panic must not reach the shared project.

It is optional in the same sense as the signing secrets: **no release job fails when it is missing.** The build logs a `::notice::` and produces installers with no ingest endpoint compiled in, so the Settings opt-in has nothing to send to and `telemetry::init` returns `None`. Nothing else about the build changes — same artifacts, same names, same signing.

Because the DSN is read at compile time, it only reaches binaries built by this workflow after the secret exists in that GitHub Environment. Add it to `dev` and `production` separately (ideally as two different Sentry projects, so alpha noise stays out of the production issue stream), and rebuild — an existing release asset cannot gain crash reporting retroactively.

The DSN is an **ingest** credential, not an API token: it can write events into the project and nothing else. It is still stored as a secret so it is masked in logs and cannot be scraped from the repo, but a leak is a spam risk, not a data-disclosure one. `SENTRY_AUTH_TOKEN` — which *can* read issues and upload debug files — is **not** used here.

See [`crash-reporting.md`](crash-reporting.md) for what the reports contain, how the scrubber works, and how to run the whole path locally.

### CI-Hub secrets CI-Capture does **not** need

| Secret / note | Why |
|---------------|-----|
| `SENTRY_AUTH_TOKEN` | CI-Capture uploads no source maps, releases or debug files — it only *ingests* events, which needs a DSN and not an auth token |
| `SENTRY_DESKTOP_DSN`, `SENTRY_FRONTEND_DSN` | Hub's own project split; CI-Capture has a single desktop project, wired as [`SENTRY_DSN`](#ci-capture-only-not-in-ci-hub-desktop-release) above |
| `SENTRY_ORG`, `SENTRY_*_PROJECT` | Metadata only in Hub vault; the org and project are already encoded in the DSN |
| `CF_R2_DL_API_TOKEN`, `CF_ACCOUNT_ID` | Hub desktop R2 upload |
| `CLOUDFLARE_API_TOKEN`, `CLOUDFLARE_ACCOUNT_ID` | Hub container/Workers deploy |
| `PORTAL_REGISTRY_USERNAME`, `PORTAL_REGISTRY_PASSWORD` | Hub container registry |
| `CI_PACKAGE_MANAGERS_TOKEN` | Hub Homebrew/Scoop publish |
| `AZURE_CLIENT_SECRET` | **Deprecated** — neither repo uses it; signing uses GitHub OIDC |

## 3. Azure federated credentials (CI-Capture-specific)

Windows signing uses **GitHub OIDC** (`permissions: id-token: write` + `azure/login@v2`). Do **not** reuse CI-Hub federated credential subjects unchanged.

In **Microsoft Entra** → your GitHub Actions app registration → **Certificates & secrets** → **Federated credentials**, add **two** credentials:

| Field | Value |
|-------|-------|
| Issuer | `https://token.actions.githubusercontent.com` |
| Audience | `api://AzureADTokenExchange` |
| Subject (dev) | `repo:companionintelligence/CI-Capture:environment:dev` |
| Subject (production) | `repo:companionintelligence/CI-Capture:environment:production` |

You can use the **same** Entra app as CI-Hub (`AZURE_CLIENT_ID` / tenant / subscription / signing account) — only the **subject** strings are repo-specific. CI-Hub keeps:

- `repo:companionintelligence/CI-Hub:environment:dev`
- `repo:companionintelligence/CI-Hub:environment:production`

Grant the service principal **Artifact Signing Certificate Profile Signer** on the signing account (same as [CI-Hub signing doc](../../ci-hub/docs/DESKTOP-RELEASE-SIGNING.md)).

### Common Azure field mix-ups

| What you might have in the vault | Maps to | Used by CI-Capture release? |
|----------------------------------|---------|----------------------------|
| Entra app **Client ID** (GUID) | `AZURE_CLIENT_ID` | Yes |
| Entra **Directory (tenant) ID** (GUID) | `AZURE_TENANT_ID` | Yes |
| Azure **Subscription ID** (GUID) | `AZURE_SUBSCRIPTION_ID` | Yes |
| **Client secret** (`4Z18Q~…` style) | — | **No** — OIDC replaces secrets |
| OAuth redirect / callback URL (e.g. `https://hub.ci.computer/api/oauth/callback`) | — | **No** — Portal/Hub login, not GitHub signing |
| Codesigning endpoint URL | `AZURE_SIGNING_ENDPOINT` | Yes — copy from CI-Hub |
| Signing account + profile names | `AZURE_SIGNING_ACCOUNT`, `AZURE_CERTIFICATE_PROFILE` | Yes — copy from CI-Hub |

### `APPLE_SIGNING_IDENTITY` format

Must be the **full codesign identity string** from Keychain or `security find-identity`, e.g.:

`Developer ID Application: LifeScope Inc. (68U45V85XX)`

It is **not** a certificate hash or UUID. If the secret is wrong or empty, the workflow picks the best `Developer ID Application` identity from the imported keychain.

## 4. Setup checklist

1. Create GitHub Environments `dev` and `production` on **CI-Capture**.
2. Copy Apple + Azure secrets from CI-Hub environments into both CI-Capture environments.
3. Add Entra federated credentials for **CI-Capture** subjects (§3).
4. Generate or locate a Linux release GPG key; store `LINUX_GPG_PRIVATE_KEY` (+ passphrase if any) in **production** (and **dev** if you sign prereleases).
5. *(Optional)* Add `SENTRY_DSN` to whichever environments should ship crash reporting. Skipping this step is a supported configuration — the build succeeds and reporting is compiled out.
6. Run **Desktop Release** → environment `dev`, tag `v0.1.0-dev.1`, prerelease ✓ — verify macOS / Windows / Linux jobs.
7. Tag `v*` push or production dispatch for a full signed release.

## 5. Password manager

Maintain a secure note from the template:

[`config/release/password-manager-note.md`](../config/release/password-manager-note.md)

Fill secret values in the vault only — never commit real credentials to git.

## Related

- [README — Cutting a release](../README.md#cutting-a-release)
- [Crash reporting](crash-reporting.md) — what `SENTRY_DSN` switches on, and what it does and does not send
- [CI-Hub desktop signing](../../ci-hub/docs/DESKTOP-RELEASE-SIGNING.md)

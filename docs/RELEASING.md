# Building & releasing CI-Applet with GitHub Actions

> **Private & Confidential — Property of Lifescope Inc. Do not distribute.**

This is the runbook for producing CI-Applet builds — the same GitHub-Actions
model CI-Hub uses (per-OS runners, code signing + notarization, environment
channels, a `latest.json` manifest, and provenance attestation). If you just
want to **produce installers to test**, jump to [Build a dev release](#1-build-a-dev-release-no-secrets-needed).

## The three workflows

| Workflow | File | Trigger | Produces |
|---|---|---|---|
| **CI** | [`ci.yml`](../.github/workflows/ci.yml) | every push to `main` + every PR | build & test on macOS/Linux/Windows — **no artifacts** |
| **Packages** | [`packages.yml`](../.github/workflows/packages.yml) | push to `main`, `v*` tags, manual | Linux container image → `ghcr.io/companionintelligence/ci-applet` |
| **Desktop Release** | [`release.yml`](../.github/workflows/release.yml) | `v*` tag push, **or** manual dispatch | native installers (`.dmg` / `.exe` / `.deb` / `.AppImage` / Arch) → a GitHub Release |

"Building these versions" = the **Desktop Release** workflow. It runs
`cargo-packager` on each OS's own runner and attaches the installers to a
GitHub Release, exactly like CI-Hub's `desktop-release.yml`.

### How it compares to CI-Hub

**Same:** per-OS installer matrix, Apple Developer-ID signing + notarization,
Windows Azure Artifact Signing, `dev`/`production` environment channels, a
generated `latest.json` manifest, SHA-256 checksums, and build-provenance
attestation. The signing secrets are **shared with CI-Hub** (same names, same
vault entries) — see [`DESKTOP-RELEASE-SECRETS.md`](DESKTOP-RELEASE-SECRETS.md).

**Not yet (deliberate):** CI-Hub also pushes `latest.json` + installers to an
R2 CDN (`dl.ci.computer`) that its in-app `updater.rs` polls for auto-update.
CI-Applet **generates** `latest.json` and attaches it to the Release, but does
not yet publish a CDN feed or self-update — that's a planned follow-up
(Phase 1). For now, updating means downloading the new installer.

---

## 1. Build a dev release (no secrets needed)

This is the fastest way to get real, downloadable installers for all three OSes
off any branch. No signing secrets are required — macOS is ad-hoc signed,
Windows/Linux are unsigned.

**Step 1 — set the version.** The workflow's preflight refuses to build unless
`Cargo.toml`'s `version` matches the tag **exactly, including the prerelease
suffix**. On your branch, bump it to the prerelease version and commit:

```toml
# Cargo.toml
version = "0.1.0-dev.1"
```

```bash
git commit -am "chore: 0.1.0-dev.1" && git push
```

**Step 2 — dispatch the workflow.** On GitHub:

**Actions** → **Desktop Release** → **Run workflow**, then set:

| Field | Value |
|---|---|
| Use workflow from | your branch |
| `tag` | `v0.1.0-dev.1` (must match `Cargo.toml`, and **must** contain a `-` suffix) |
| `prerelease` | `true` |
| `environment` | `dev` |
| `platforms` | `all` (or `macos-only` / `linux-only` / `windows-only`) |

**Result:** a **prerelease** GitHub Release tagged `v0.1.0-dev.1` with every
installer, `latest.json`, and `SHA256SUMS`. Grab the installers from the
Release page (or from each job's **Artifacts**).

> **Rules the workflow enforces for dev runs** (it will fail fast, with a clear
> error, otherwise): the tag must be a prerelease (contain `-`), `prerelease`
> must be `true`, and `Cargo.toml` must match the tag version. This keeps dev
> runs from ever consuming a real `v0.1.0` production tag.

---

## 2. Cut a production release

Production builds are **tag-driven** and require the full signing secrets to be
configured (the workflow refuses to produce an unsigned production build).

```bash
# 1. Bump Cargo.toml to the release version and commit it to main
#    version = "0.1.0"
git commit -am "chore: release 0.1.0"

# 2. Tag that commit and push the tag
git tag v0.1.0
git push origin v0.1.0
```

Pushing a `v*` tag automatically runs Desktop Release with `environment:
production` and `platforms: all`. A tag **without** a `-` suffix (`v0.1.0`)
publishes a full release; a tag **with** one (`v0.1.0-beta.1`) publishes a
prerelease.

You can also trigger a production build manually — but you must select the
**tag ref** (not a branch) in "Use workflow from", or preflight rejects it
("Production desktop releases must run from the matching release tag ref").

**Signing requirements** (validated at build time for `production`):

- **macOS** — `APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_ID`,
  `APPLE_PASSWORD`, `APPLE_TEAM_ID` → Developer-ID signed **and** notarized.
- **Windows** — the `AZURE_*` Artifact-Signing secrets → signed `.exe`.
- **Linux** — optional `LINUX_GPG_PRIVATE_KEY` → detached `.asc` signatures.

Set these as **GitHub Environment secrets** on the `production` (and `dev`)
environments. Full inventory, CI-Hub copy list, and the Azure OIDC federated-
credential subjects are in
[`DESKTOP-RELEASE-SECRETS.md`](DESKTOP-RELEASE-SECRETS.md).

---

## What a release contains

Every Desktop Release publishes:

- **Installers** — `.dmg` (macOS arm64 + Intel), `-setup.exe` (Windows NSIS),
  `.deb` + `.AppImage` + Arch `pacman` package (Linux x86_64), all built
  `--features rich-extractors`.
- **`latest.json`** — machine-readable manifest (version, channel, per-asset
  size + sha256, source commit, workflow run URL).
- **`CI-Applet-<tag>-SHA256SUMS.txt`** — checksums for every asset.
- **`.asc`** detached GPG signatures (when `LINUX_GPG_PRIVATE_KEY` is set).
- **Build-provenance attestations** (`actions/attest-build-provenance`).

### Verify a download

```bash
shasum -a 256 -c CI-Applet-v0.1.0-SHA256SUMS.txt
```

---

## Signing tiers at a glance

| Secrets configured | macOS | Windows | Linux |
|---|---|---|---|
| none (dev) | ad-hoc signed (Gatekeeper warns; right-click → Open) | unsigned (SmartScreen warns) | unsigned |
| Apple only | Developer-ID signed + notarized | unsigned | unsigned |
| Apple + Azure | signed + notarized | Azure-signed | unsigned |
| + `LINUX_GPG_PRIVATE_KEY` | signed + notarized | Azure-signed | detached `.asc` |

Production **requires** at least the Apple + Azure tiers for the platforms it
builds; dev needs nothing.

---

## Build installers locally (one-off, no CI)

```bash
cargo install cargo-packager --locked
# macOS: ad-hoc sign so the .app opens without a Developer-ID cert
APPLE_SIGNING_IDENTITY=- cargo packager --release --formats dmg
# Linux / Windows: pick the formats for your OS
cargo packager --release --formats deb,appimage   # Linux
cargo packager --release --formats nsis            # Windows
```

Installer configuration lives in `[package.metadata.packager]` in
[`Cargo.toml`](../Cargo.toml); icons are in `packaging/icons/`.

---

## Troubleshooting

| Error in the run | Cause / fix |
|---|---|
| `Cargo.toml version (X) must match release tag version (Y)` | Bump `version` in `Cargo.toml` to match the tag (including any `-dev.N`) and commit before dispatching. |
| `Non-production desktop releases must use a prerelease tag such as v0.1.0-dev.1` | Add a `-` suffix to the dev tag. |
| `Non-production desktop releases must be marked as prerelease` | Set the `prerelease` input to `true`. |
| `Production desktop releases must run from the matching release tag ref` | For a manual production run, select the **tag** in "Use workflow from", or just push the tag. |
| `Production macOS releases require APPLE_*` / `... Azure Artifact Signing secrets` | Configure the signing secrets on the `production` environment, or use `environment: dev` for an unsigned build. |
| macOS won't open the downloaded app | Expected for dev/ad-hoc builds — right-click → **Open** once, or notarize via a production release. |

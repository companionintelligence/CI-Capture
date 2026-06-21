# install.sh

## Purpose
Installs a prebuilt `vz` binary from GitHub Releases into a user-local bin directory. This provides a no-Rust install path for supported macOS and Linux targets.

## Components

### `detect_target`
- **Does**: Maps the local OS/architecture to a supported release asset target triple.
- **Interacts with**: `uname`.

### `download_url`
- **Does**: Builds the GitHub Releases download URL for the selected target and version.
- **Interacts with**: Release asset names produced by `.github/workflows/build-release.yml`.

### Main install flow
- **Does**: Downloads, extracts, and installs `vz` into `INSTALL_DIR`.
- **Interacts with**: `curl`, `tar`, `install`, and shell `PATH`.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| End users | `sh scripts/install.sh` installs `vz` to `~/.local/bin` by default | Changing defaults, removing supported targets |
| README | Environment variables (`VZ_REPO`, `VZ_VERSION`, `INSTALL_DIR`) remain valid, including `VZ_VERSION=rolling` | Renaming env vars or behavior |
| CI release workflow | Release archives are named `vz-<target>.tar.gz` | Asset naming changes |

## Notes
This script intentionally supports only the targets built by CI. Unsupported platforms fail fast with a clear message instead of guessing at incompatible binaries. On macOS, that currently means Apple Silicon only.

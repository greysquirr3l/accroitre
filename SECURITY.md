# Security policy

## Supported versions

| Version | Supported |
|---|---|
| 0.1.x   | ✅ Active |

Accroître is pre-1.0; security patches will be backported to the current minor version.

## Reporting a vulnerability

**Do not open a public GitHub issue for security-sensitive bugs.**

Email security reports to: **<nick.campbell@protonmail.com>** (PGP key on request).

Please include:

- A clear description of the vulnerability and its impact
- Steps to reproduce, or a proof-of-concept
- Affected versions (commit SHA or version tag)
- Any known workarounds

You should receive an acknowledgement within 72 hours. Critical issues get a fix and an advisory within 7 days; lower-severity issues follow the normal release cadence.

## Threat model

Accroître is a file copy tool. The security boundary it cares about is:

- **Path traversal**: source/destination paths must not escape their intended root. (Currently mitigated by operator-supplied paths; future hardening will reject `..` and absolute path components in destinations.)
- **SSH authentication**: keys, passphrases, and passwords are loaded into memory only for the duration of the connection and are not logged. `tracing` filters must never include auth fields.
- **Self-update**: GitHub release downloads are verified against SHA-256 checksums published alongside the release. Checksum verification is constant-time.
- **Manifest + cache atomicity**: writes go through temp-file + rename. The `.accroitre.lock` cross-process lock prevents concurrent runs from clobbering each other.
- **Supply chain**: `cargo deny` enforces license + advisory policy on every PR. Releases are reproducible via `Cargo.lock`.

Out of scope:

- Operator-side misconfiguration (e.g., copying into a path the operator didn't intend)
- Filesystem-level bugs in the underlying OS
- Compromise of GitHub or the crates.io registry themselves

## Hardening guidance for downstream users

- Pin the version in CI: `cargo install accroitre-cli --version 0.1.0 --locked`
- Run `cargo install` from a clean `Cargo.lock`, not `cargo install accroitre-cli` alone (the latter resolves fresh)
- For high-assurance deployments, build from source and verify the SHA-256 of the binary against the value published in the GitHub release
- Use SSH key auth (`AuthMethod::KeyFile`) over password auth where possible
- Run `cargo deny` against your own lockfile if you embed `accroitre` as a library dependency
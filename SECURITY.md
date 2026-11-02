# Security policy

## Reporting a vulnerability

**Please don't open a public issue for security problems.** Report them privately through
[GitHub's private vulnerability reporting](https://github.com/jakeayy/kick-level-farm/security/advisories/new).

Please include what you found, how to reproduce it, and what an attacker could do with it.
You'll get a reply as soon as possible. The fix and an advisory are published together once a
patched release is out.

**In scope**, for example:

- Leaks of the Kick `session_token`, Discord webhook URLs or other user data (in logs,
  notifications, network traffic, files, or the Docker image).
- Problems in the GitHub workflows, for example ways to make the automated triage run
  untrusted code, leak secrets, or push to the repository.
- Vulnerabilities in the release binaries or Docker image (including outdated dependencies with
  known advisories).

**Out of scope:** Kick banning or flagging accounts that use this tool. That's an accepted risk
of using it, as described in the README.

## Supported versions

Only the latest stable release and the current `main` branch (the `dev-…` pre-release and
the `latest` Docker image) get security fixes.

## Automated checks

- **CodeQL** scans the Rust code and the GitHub Actions workflows on every relevant change and
  weekly.
- **cargo-deny** checks dependencies daily against the RustSec advisory database and checks
  license compatibility.
- **Dependabot** opens update PRs for crates, Actions and Docker base images, and alerts on
  vulnerable dependencies.
- **Secret scanning** with push protection blocks commits that contain known token formats.
- **Signed build provenance** for every release download and Docker image. Check one with
  `gh attestation verify <file> --repo jakeayy/kick-level-farm`, or for the image
  `gh attestation verify oci://ghcr.io/jakeayy/kick-level-farm:latest --repo jakeayy/kick-level-farm`.

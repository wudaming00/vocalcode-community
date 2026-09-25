# Security and privacy

## Reporting

Use [GitHub private vulnerability reporting](https://github.com/wudaming00/vocalcode-community/security/advisories/new)
for this repository. It is enabled; a GitHub account is required to submit a
private report. See [MAINTAINERS.md](MAINTAINERS.md) for project identity.
Do not post credentials, licence keys, personal recordings, or exploit-bearing
user data in public issues.

Include the affected version, reproduction steps, impact, and a minimal
synthetic example. Do not access someone else's data to demonstrate a bug.

## Boundaries worth testing

- UI/native IPC validation and supported message types.
- Focus identity and foreground-window checks before text insertion.
- Download origins, hashes, archive extraction, and update signatures.
- Local file permissions, symlink/reparse-point handling, and data removal.
- Recording consent, cancellation, diagnostic exports, and retention controls.
- Optional rewrite providers: no silent cloud fallback; explicit per-request
  consent; native CLI resolution without shell shims; stdin-only source text;
  bounded subprocesses; no tools/MCP/browser integration; stale preview and
  undo protection. An external CLI remains a separately trusted program, not
  an OS-sandboxed text processor. See [the provider boundaries](docs/SMART-REWRITE.md).

The free build changes product access, not these security boundaries. It does
not introduce a universal licence key, weaken receipt verification in the old
paid build, or turn off the updater's signature verification. It never reads
the paid releases' licence, trial or time-anchor files, and its updater accepts
only this repository's signed releases.

## Release policy

This is an early source preview, without a guaranteed security-support schedule.
Production credentials must never be available to pull-request workflows.
Use hosted, ephemeral CI runners for untrusted code, not the maintainer's
signed-in development machine or production self-hosted release runner.

The initial source review upgraded Rustls to 0.23.45 for RUSTSEC-2026-0285.
`cargo audit` reported no known vulnerabilities on 2026-09-22, with warnings
for the unmaintained `proc-macro-error` dependency and Linux-side `glib`
unsoundness still visible. Linux is not a supported build in this preview.
This dated check is not a guarantee against future advisories; CI reruns it.

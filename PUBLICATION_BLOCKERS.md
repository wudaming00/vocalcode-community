# Source preview and binary-release readiness

This repository is an **early public source preview**, authorized by the owner
on 2026-09-22. It is not a production-ready binary release. The source manifest
distinguishes `source_publication_authorized: true` from
`binary_release_ready: false`; passing automated tests cannot certify native
audio behavior, ownership of unknown future contributions, or installer safety.

## Initial source-publication review

- [x] Owner selected AGPL-3.0-only with commercial-use rights explained and
      authorized a new public repository, without exposing private Git history.
- [x] First-party crates and docs carry the selected licence; third-party
      code/model notices retain their own terms. Source review does not grant
      rights over unexported backends, customer records, or model weights.
- [x] Exported files were reviewed with a manifest, pinned Gitleaks, and a
      separate location-only private-path/credential check. No live credentials
      or user recordings were found. This is not a guarantee against every
      possible secret or a substitute for reviewing future changes.
- [x] Rustls was updated to 0.23.45; the dependency inventory was regenerated.
      The dependency audit reports zero vulnerabilities and two follow-up
      warnings (`proc-macro-error` and Linux-side `glib`).
- [x] A fresh public repository and GitHub private vulnerability reporting
      were configured; contact routes are in MAINTAINERS.md and SECURITY.md.
- [x] GitHub-hosted Windows/macOS test/build and Linux security checks are
      configured with read-only repository permissions and no production keys.
      Existing meeting, correction, migration and dictation UI tests are included.

## Required before a production-ready installer

- [ ] Complete clean-user Windows and native Apple-silicon audio, permission,
      focused-input, installation, and recovery tests. Hosted CI is not a
      substitute for these tests, even when it passes.
- [ ] Separate community data, single-instance identity, autostart, and update
      channels, and test an explicit safe migration. Until then, use a separate
      OS user for testing and do not overwrite an existing installation.
- [ ] Remove remaining Pro/online-licensing labels from community screens.
- [ ] Resolve or document target-specific dependency warnings and continue
      maintaining automated security/dependency checks.
- [ ] Review exact model/native-runtime redistribution terms, all artwork,
      and platform-signing/notarization requirements for packaged binaries.
- [ ] Publish real synthetic-content screenshots and a supported download path.
- [ ] Pair each binary with its exact Corresponding Source and required
      build/install files and notices. Developer CI artifacts are unsigned,
      not a supported installer/update channel.
- [ ] Decide and communicate treatment of previous paid customers separately;
      this source release does not change live payments or promise refunds.

## Excluded from this repository

Private Git history, production deployment workflows, commerce backends,
signing private keys/certificates, operator credentials, purchase/tester
ledgers, private marketing records, user audio/transcripts/diagnostics, and
build outputs. The public receipt-contract fixture is test data required by
desktop client tests, not the payment service or a production signing key.

These are maintainer release procedures, not additional restrictions on AGPL
rights. Community support is best-effort; no legal or security certification
or service-level agreement is implied.

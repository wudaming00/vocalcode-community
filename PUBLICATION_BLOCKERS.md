# Community release validation and remaining limitations

Source publication and the full community release workflow were authorized by
the owner on 2026-09-22. Signed installers are published only by the gated
[release workflow](.github/workflows/community-release.yml); check its actual
run result and the [release assets](https://github.com/wudaming00/vocalcode-community/releases)
for availability. Passing automated checks cannot certify every native audio
device, permission flow, or future contribution. The original `source-preview-*`
release remains source-only and is not the desktop installer.

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

## Implemented release safeguards

- [x] Separate community installation, data, instance, autostart, updater and
      update staging identities. No silent legacy migration or data deletion.
- [x] Remove community Pro badges and paid activation flows; all local features
      are available without an account or licence server.
- [x] Build without credentials, isolate signing in a protected environment,
      verify on fresh credential-free runners, and publish only after both OS
      verification jobs pass. Actions and Inno Setup are pinned.
- [x] Sign Windows app, uninstaller and installer; notarize/staple macOS app
      and DMG. Check publisher, edition and version before installation.
- [x] Bundle required notices and exact source; generate SHA-256/size-bound
      manifests from the verified assets. Do not overwrite released bytes.
- [x] Test Windows install/reinstall/uninstall with synthetic preserved data,
      macOS Gatekeeper and native runtime loading. These are workflow gates;
      implementation alone is not evidence that a particular run succeeded.

## Device QA and follow-up (not claimed complete)

- [ ] Complete clean-user Windows and native Apple-silicon audio, permission,
      focused-input, installation, and recovery tests. Hosted CI is not a
      substitute for these tests, even when it passes.
- [ ] Validate explicit legacy data migration and cross-version upgrades.
      Current installer smoke tests cover same-version reinstall only.
- [ ] Resolve or document target-specific dependency warnings and continue
      maintaining automated security/dependency checks.
- [ ] Continue reviewing model/native-runtime redistribution terms when
      dependencies change; the packaged Windows runtime inventory records exact
      shipped DLL versions and hashes. Model weights are downloaded separately.
- [ ] Add real synthetic-content screenshots across supported devices.
- [ ] Decide and communicate treatment of previous paid customers separately;
      this source release does not change live payments or promise refunds.

## Excluded from this repository

Private Git history, legacy commerce deployment workflows, commerce backends,
signing private keys/certificates, operator credentials, purchase/tester
ledgers, private marketing records, user audio/transcripts/diagnostics, and
build outputs. The public receipt-contract fixture is test data required by
desktop client tests, not the payment service or a production signing key.

These are maintainer release procedures, not additional restrictions on AGPL
rights. Community support is best-effort; no legal or security certification
or service-level agreement is implied.

# Building and releasing VocalCode Community

These instructions prepare an unsigned, local development build. They do not
publish a release or bypass platform permissions. First-party desktop source
uses AGPL-3.0-only; read [LICENSE](LICENSE) and [LICENSING.md](LICENSING.md).

## Prerequisites

- Rust toolchain pinned in `rust-toolchain.toml` (currently 1.97.1).
- Git and network access for Cargo dependencies and pinned native libraries.
- Node.js with `node:test` for the UI contract tests (Node 22 or newer).
- Python 3.11 or newer for packaging helpers (CI pins 3.14.4).
- Sufficient disk for Cargo output and separately downloaded ASR models.

### Windows x64

Install Visual Studio C++ Build Tools, the Windows SDK/resource compiler,
LLVM/libclang, and the Microsoft Edge WebView2 Runtime. Build from a developer
PowerShell environment where the native compiler and Windows SDK are usable.
`.cargo/config.toml` defaults `LIBCLANG_PATH` to `C:\Program Files\LLVM\bin`;
set the variable explicitly if LLVM is elsewhere.

```powershell
cargo test --workspace --locked --features vocalcode-app/community
cargo build -p vocalcode-app --release --locked --features community
```

The executable is `target/release/vocalcode-app.exe`. Keep the downloaded
sherpa/ONNX runtime DLLs adjacent to it; the executable alone is not a portable
distribution. An installed VC++ runtime may also be required. Never blindly
copy a developer machine's DLLs into an installer: check redistribution terms.

### Apple-silicon macOS

Install Xcode Command Line Tools and use a native arm64 shell. Override the
Windows-only default libclang path before building:

```sh
export LIBCLANG_PATH="$(xcode-select -p)/usr/lib"
export MACOSX_DEPLOYMENT_TARGET=11.0
cargo test --workspace --locked --features vocalcode-app/community
cargo build -p vocalcode-app --release --locked --features community
```

The dependency must find libclang under that path; an Xcode installation may
need its toolchain `usr/lib` instead. Runtime dylibs must remain adjacent to
the binary. The repository's rpaths cover that layout. This command is not an
`.app` packaging/signing/notarization workflow. Microphone, Accessibility,
Input Monitoring, and relevant system-audio permissions still apply.

## Verification

```sh
cargo fmt --all -- --check
cargo test --workspace --locked --features vocalcode-app/community
cargo clippy --workspace --all-targets --locked --features vocalcode-app/community -- -D warnings
node --test packaging/community/test-community-ui.mjs
node --test packaging/community/test-licensing.mjs
node --test packaging/release/test_replay_metrics.mjs
python -m unittest discover -s packaging/community -p 'test_*.py' -v
node --test packaging/release/test_calendar_ui.mjs packaging/release/test_control_bar_ui.mjs packaging/release/test_correction_review_ui.mjs packaging/release/test_filler_history_ui.mjs packaging/release/test_meeting_prompt_ui.mjs packaging/release/test_meeting_ui.mjs packaging/release/test_migration_ui.mjs packaging/release/test_new_install_defaults_ui.mjs packaging/release/test_noise_filter_ui.mjs packaging/release/test_workflow_ui.mjs
```

Ignored tests are opt-in: some load large ASR models, capture devices, install
global hooks, or inject text. Do not run every ignored test on an active desktop.
`--offline` is useful after dependencies are cached; it does not make an empty
machine self-contained, nor prohibit a native build script from downloading.

## Optional hidden UI checks

Windows-only, debug-build UI probes can be run without opening a recording or
the installed app. They use isolated temporary WebView profiles and leave the
native windows hidden:

```powershell
cargo run --locked -p vocalcode-app --features community --example control_bar_hidden_smoke
cargo run --locked -p vocalcode-app --features community --example control_bar_ipc_smoke
cargo run --locked -p vocalcode-app --features community --example indicator_hidden_smoke
cargo run --locked -p vocalcode-app --features community --example webui_hidden_smoke -- en
cargo run --locked -p vocalcode-app --features community --example webui_hidden_smoke -- zh
```

These exercise layout, native window flags and programmatic IPC, not real mouse
input, microphones, speech quality or cross-application text injection. Do not
interpret hidden-window checks as completed end-to-end desktop acceptance.

## Edition boundary

The exported candidate defaults to `community`. In the original private
repository this feature is opt-in, so preparing it cannot silently change the
existing paid release. Community builds:

- allow all local workflows without a receipt or account;
- skip trial/licence maintenance before device identification or user-data IO;
- refuse purchase/activation/recovery commands and paid-channel update URLs;
- use an independent GitHub Releases updater with publisher/edition verification;
- build in release mode without a licence public-key environment variable;
- preserve native input safety and the legacy updater's signature checks.

No production signing private key, payment credential, R2 credential, or
licensing backend is required. Public receipt fixtures are test data, not
production signing material. Official release credentials are held in the
protected `community-release` GitHub environment, never in source or PR jobs.

If you redistribute binaries, provide their Corresponding Source under AGPL
section 6, including the build/install scripts needed for your version. Retain
third-party notices and meet the separately applicable runtime/model terms.
The corresponding source for a CI artifact is its exact commit in
[this repository](https://github.com/wudaming00/vocalcode-community). Artifact
names include the commit SHA; use GitHub's source archive at that commit,
not an unrelated newer or older revision. Stable release assets include an
explicit Corresponding Source archive of the exact release commit.

## GitHub-hosted continuous integration

Pushes to `main`, pull requests, and manual dispatches run the
[Community checks workflow](https://github.com/wudaming00/vocalcode-community/actions/workflows/community-ci.yml).
It uses standard GitHub-hosted `windows-2025`, `macos-15` (Apple silicon), and
`ubuntu-24.04` runners, not the maintainer's own machines. Windows/macOS jobs
test, lint and build unsigned executables; the Linux job audits dependencies
and scans checked-out files for secrets without building the desktop app.

Non-PR builds retain unsigned developer artifacts for three days. These are
not signed installers or a macOS `.app` bundle. Keep runtime libraries next to
the executable. No production secrets or code-signing keys are available to
these CI jobs; the separately protected release workflow handles signing.
Standard public-repository runner time is free under GitHub's current terms;
artifact storage has separate limits.

## Signed releases and automatic updates

The [Signed community release workflow](.github/workflows/community-release.yml)
uses only GitHub-hosted runners. To release as the repository owner:

1. Review and push the version/source change to `main`. Wait for **Community
   checks** to pass on that exact commit.
2. Dispatch **Signed community release** from `main`; set `publish` to `true`
   to publish after all gates, or leave it off for a signing/install rehearsal.
   Packaging consumes the exact successful main CI run's commit-named binaries
   and rechecks native linkage. If its three-day artifacts have expired, rerun
   Community checks on `main` first.
3. Approve the `community-release` environment for signing. Windows app,
   uninstaller and installer are individually signed through Azure Artifact
   Signing. macOS app and DMG are signed, notarized and stapled using a temporary
   keychain, removed at the end of the signing step.
4. Fresh runners without signing credentials verify both packages. Windows
   installs, reinstalls the same version and uninstalls while checking preserved
   synthetic data; macOS verifies Gatekeeper, notarization and mounted identity.
   Both execute `--build-info` to check native linkage without starting capture.
5. Approve publication. Only then does a draft become a stable release, exposing
   both installers, exact source, checksums and `latest.json` together.

Version tags must match Cargo's version and current `main`. Existing release
assets are never overwritten; bump the version for changed binaries. Only the
publish job has repository write permission. Build and verification jobs have
no signing secrets. Action revisions and the Inno Setup compiler are pinned.

The protected environment requires the maintainer's approval and restricts refs
to `main` and `v*`. Its eight secrets are `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET`,
`AZURE_TENANT_ID`, `MACOS_CERT_P12`, `MACOS_CERT_PASSWORD`, `NOTARY_KEY_P8`,
`NOTARY_KEY_ID`, and `NOTARY_ISSUER`. Never place these in repository files or
ordinary workflow environment variables. Forks must configure their own identity,
signing services and update channel; they cannot publish as the original project.

The updater checks this repository's `releases/latest/download/latest.json`.
Bounded HTTPS redirects are restricted to GitHub release asset hosts; exact
versioned download URLs, size, SHA-256 and OS publisher/edition identities are
validated before installation. The legacy paid channel remains separate.

These checks do not establish real microphone quality, meeting echo performance,
all OS permission flows, or a cross-version data migration. Those require
device testing; see [validation scope](PUBLICATION_BLOCKERS.md).

## Data isolation and models

Community data lives in `%LOCALAPPDATA%\VocalCode Community` on Windows and
`~/Library/Application Support/VocalCode Community` on macOS. The bundle ID is
`app.vocalcode.Community`; instance, autostart and update staging identities are
also edition-specific. Legacy data is not automatically migrated or deleted.
Close any other VocalCode edition before use to avoid competing global hooks.
Building and running non-ignored unit tests does not install the app or replace
user data. Installer smoke tests refuse to run outside disposable hosted CI.

The small Silero VAD artifact is bundled with its MIT notice and pinned hash.
Recognition model weights are not in this source snapshot. The app downloads
the selected model on setup; each model retains its own terms and attribution.

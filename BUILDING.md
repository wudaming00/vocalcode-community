# Building the community preview

These instructions prepare an unsigned, local development build. They do not
publish a release or bypass platform permissions. First-party desktop source
uses AGPL-3.0-only; read [LICENSE](LICENSE) and [LICENSING.md](LICENSING.md).

## Prerequisites

- Rust toolchain pinned in `rust-toolchain.toml` (currently 1.97.1).
- Git and network access for Cargo dependencies and pinned native libraries.
- Node.js with `node:test` for the UI contract tests (Node 22 or newer).
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

### Apple-silicon macOS (validation pending)

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
node --test packaging/release/test_calendar_ui.mjs packaging/release/test_correction_review_ui.mjs packaging/release/test_filler_history_ui.mjs packaging/release/test_meeting_prompt_ui.mjs packaging/release/test_meeting_ui.mjs packaging/release/test_migration_ui.mjs packaging/release/test_noise_filter_ui.mjs packaging/release/test_workflow_ui.mjs
```

Ignored tests are opt-in: some load large ASR models, capture devices, install
global hooks, or inject text. Do not run every ignored test on an active desktop.
`--offline` is useful after dependencies are cached; it does not make an empty
machine self-contained, nor prohibit a native build script from downloading.

## Edition boundary

The exported candidate defaults to `community`. In the original private
repository this feature is opt-in, so preparing it cannot silently change the
existing paid release. Community builds:

- allow all local workflows without a receipt or account;
- skip trial/licence maintenance before device identification or user-data IO;
- refuse purchase/activation/recovery and paid-update IPC commands;
- build in release mode without a licence public-key environment variable;
- preserve native input safety and the legacy updater's signature checks.

No production signing private key, payment credential, R2 credential, or
licensing backend is required. Public receipt fixtures are test data, not
production signing material. Official release signing stays outside this
candidate. Do not advertise a community signed updater until a separate
reviewed channel exists.

If you redistribute binaries, provide their Corresponding Source under AGPL
section 6, including the build/install scripts needed for your version. Retain
third-party notices and meet the separately applicable runtime/model terms.
The corresponding source for a CI artifact is its exact commit in
[this repository](https://github.com/wudaming00/vocalcode-community). Artifact
names include the commit SHA; use GitHub's source archive at that commit,
not an unrelated newer or older revision. Official installers are not published.

## GitHub-hosted continuous integration

Pushes to `main`, pull requests, and manual dispatches run the
[Community checks workflow](https://github.com/wudaming00/vocalcode-community/actions/workflows/community-ci.yml).
It uses standard GitHub-hosted `windows-2025`, `macos-15` (Apple silicon), and
`ubuntu-24.04` runners, not the maintainer's own machines. Windows/macOS jobs
test, lint and build unsigned executables; the Linux job audits dependencies
and scans checked-out files for secrets without building the desktop app.

Non-PR builds retain unsigned developer artifacts for three days. These are
not signed installers or a macOS `.app` bundle. Keep runtime libraries next to
the executable; review the data-isolation warning before running it. No
production secrets or code-signing keys are configured in these workflows.
Standard public-repository runner time is free under GitHub's current terms;
artifact storage has separate limits. Signed releases need a separate review.

## Data isolation and models

For now the preview uses the same application data directory as the existing
desktop app. Use a separate OS user for a clean experiment. Building and running
automated non-ignored tests does not install the app or replace user data.

The small Silero VAD artifact is bundled with its MIT notice and pinned hash.
Recognition model weights are not in this source snapshot. The app downloads
the selected model on setup; each model retains its own terms and attribution.

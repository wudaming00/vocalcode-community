# Building and releasing VocalCode

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
cargo clippy --workspace --all-targets --locked --no-default-features -- -D warnings
node --test packaging/community/test-community-ui.mjs
node --test packaging/community/test-licensing.mjs
node --test packaging/release/test_replay_metrics.mjs
python -m unittest discover -s packaging/community -p 'test_*.py' -v
node --test "packaging/release/test_*.mjs"
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

## The free build

VocalCode is free and open source. The `community` build feature, on by
default, is that free build; without it the code base still compiles the old
paid build, which is kept only so its tests keep passing and is never released.
The free build:

- allows every workflow without a receipt, account or activation;
- never reads licence, trial or time-anchor files and skips licence maintenance;
- refuses purchase/activation/recovery commands and paid-channel update URLs;
- updates from this repository's GitHub Releases with publisher and product
  verification;
- builds in release mode without a licence public-key environment variable;
- keeps native input safety and the updater's signature checks.

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
[Checks workflow](https://github.com/wudaming00/vocalcode-community/actions/workflows/community-ci.yml).
It uses standard GitHub-hosted `windows-2025`, `macos-15` (Apple silicon), and
`ubuntu-24.04` runners, not the maintainer's own machines. Windows/macOS jobs
test, lint and build unsigned executables; the Linux job audits dependencies
and scans checked-out files for secrets without building the desktop app.
A newer push to a pull request cancels that pull request's older run; runs on
`main` are never cancelled, because a signed release requires a successful
push run on its exact commit. Third-party dependency builds are cached from
`main` only, keyed on `Cargo.lock` and `rust-toolchain.toml`; workspace and
vendored crates are always rebuilt.

A further Windows job (`e2e-windows`) installs the real, signed paid
VocalCode 1.2.1 and VocalCode Community 1.4.0 installers (sha-pinned, cached)
on a disposable runner, gives them realistic data, and then installs this
commit's unsigned `VocalCodeSetup.exe`: over the paid app exactly as that
app's own updater runs it, and over the early free build. It checks the
registration, program files, login items, that every seeded file (including
licence-named decoys) is untouched, that the new build started and loads
that data, and that uninstalling keeps it. See
`packaging/community/e2e-windows.ps1`.

Non-PR builds retain unsigned developer artifacts for three days. These are
not signed installers or a macOS `.app` bundle. Keep runtime libraries next to
the executable. No production secrets or code-signing keys are available to
these CI jobs; the separately protected release workflow handles signing.
Standard public-repository runner time is free under GitHub's current terms;
artifact storage has separate limits.

## Signed releases and automatic updates

The [Signed release workflow](.github/workflows/community-release.yml)
uses only GitHub-hosted runners. To release as the repository owner:

1. Review and push the version/source change to `main`. Wait for **Checks** to
   pass on that exact commit.
2. Dispatch **Signed release** from `main`; set `publish` to `true`
   to publish after all gates, or leave it off for a signing/install rehearsal.
   Packaging consumes the exact successful main CI run's commit-named binaries
   and rechecks native linkage. If its three-day artifacts have expired, rerun
   Checks on `main` first.
3. Approve the `community-release` environment for signing. Windows app,
   uninstaller and installer are individually signed through Azure Artifact
   Signing. macOS app and DMG are signed, notarized and stapled using a temporary
   keychain, removed at the end of the signing step.
4. Fresh runners without signing credentials verify both packages. Windows
   installs, reinstalls the same version and uninstalls while checking preserved
   synthetic data; macOS verifies Gatekeeper, notarization and mounted identity.
   Both check exactly what a paid VocalCode's updater checks before it runs
   them (see below).
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
versioned download URLs, size, SHA-256 and OS publisher/product identities are
validated before installation.

### Paid installations

The paid releases (up to 1.2.1) read `https://vocalcode.app/latest.json`. They
offer an update only if it is newer, the licence is valid or the trial active,
and the URL is exactly `https://vocalcode.app/VocalCodeSetup.exe` (Windows) or
`https://vocalcode.app/VocalCode-<version>.dmg` (macOS), with the lowercase
SHA-256 and size of that file. Before running it they require:

- Windows: Authenticode status `Valid`, signer exactly
  `CN=Daming Wu, O=Daming Wu, L=Newberry, S=FL, C=US`, ProductVersion equal to
  the version (a trailing `.0` allowed), ProductName `VocalCode`,
  OriginalFilename `VocalCodeSetup.exe`. The installer then runs with
  `/VERYSILENT /SUPPRESSMSGBOXES /NOCANCEL /NORESTART` and the app restarts
  `{app}\VocalCode.exe`.
- macOS: the DMG signed by Team `58Y98W3QQK` and notarized, `VocalCode.app` at
  its root with bundle identifier `app.vocalcode.VocalCode`, a designated
  requirement naming that identifier and team, and
  `CFBundleShortVersionString` equal to the version. The app swaps its own
  bundle for it and relaunches `Contents/MacOS/VocalCode`.

The release's `VocalCodeSetup.exe` and `VocalCode-<version>.dmg` are built to
pass exactly these checks, so the website only has to serve those same bytes
under those names and list their SHA-256 and size.

These checks do not establish real microphone quality, meeting echo performance,
all OS permission flows, or a cross-version data migration. Those require
device testing; see [validation scope](PUBLICATION_BLOCKERS.md).

## Data and models

VocalCode keeps its data in `%LOCALAPPDATA%\VocalCode` on Windows and
`~/Library/Application Support/VocalCode` on macOS, the folders the paid
releases used, with the same bundle ID (`app.vocalcode.VocalCode`), executable
(`VocalCode.exe`), login item (`VocalCode`), running-copy mutex
(`Local\VocalCode.Desktop`) and installer registration (`VocalCode_is1`). A
paid installation that updates therefore keeps everything in place; the free
build ignores the paid licence, trial and time-anchor files and never opens
them. The installer overwrites the paid uninstall log, so uninstalling
VocalCode later keeps the data folder (the paid uninstaller purged it).

The early free builds (VocalCode Community 1.3.1 and 1.4.0) used their own
names (`VocalCode Community`, `app.vocalcode.Community`,
`VocalCodeCommunity.exe`). The Windows installer runs that app's uninstaller,
which keeps its data folder; **Settings → System → Previous VocalCode** copies
from it. Don't run two copies at once: each hooks the talk key.

Unit tests never touch those locations: a test build keeps its data folder in
a temporary directory and never writes the login item. Installer and
end-to-end tests refuse to run outside disposable hosted CI.

The small Silero VAD artifact is bundled with its MIT notice and pinned hash.
Recognition model weights are not in this source snapshot. The app downloads
the selected model on setup; each model retains its own terms and attribution.

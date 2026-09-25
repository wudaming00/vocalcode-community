<p align="center">
  <img src="docs/assets/vocalcode-community.svg" alt="VocalCode — Your voice. Your machine. Local dictation and meeting notes." width="100%" />
</p>

<p align="center">
  <strong>Speak naturally. Keep your work local.</strong><br />
  CPU-first dictation and meeting notes for Windows and Apple-silicon Mac.
</p>

<p align="center">English · <a href="README.zh-CN.md">简体中文</a></p>

<p align="center">
  <a href="#what-you-can-do">Features</a> ·
  <a href="BUILDING.md">Build from source</a> ·
  <a href="#privacy-and-network-access">Privacy</a> ·
  <a href="MAINTAINERS.md">Maintainer & contact</a> ·
  <a href="CONTRIBUTING.md">Contribute</a>
</p>

<p align="center">Maintained by <a href="https://github.com/wudaming00">Daming Wu</a> · <a href="LICENSE">AGPL-3.0-only</a></p>

> **Free community edition, with all local features and no activation.**
> [Downloads and release status](https://github.com/wudaming00/vocalcode-community/releases)
> include signed Windows installers and notarized Apple-silicon DMGs only after
> the release checks pass. Tags named `source-preview-*` contain source only.
> See [validation scope and limitations](PUBLICATION_BLOCKERS.md).

[![Community checks](https://github.com/wudaming00/vocalcode-community/actions/workflows/community-ci.yml/badge.svg)](https://github.com/wudaming00/vocalcode-community/actions/workflows/community-ci.yml)

## Why I'm building this

I'm **Daming Wu**, the maintainer of VocalCode. I started with a simple need:
speaking longer prompts into coding tools instead of typing every word. The
project now also explores local meeting notes, reusable vocabulary, and
recoverable dictation history.

I want recognition to run on my own machine, with CPU-friendly options and
clear controls over what is recorded and retained. Opening the source is a
way to develop that in public: reproducible bug reports, real language
feedback, and improvements that others can inspect and share.

**Help shape the original project.** Multilingual test cases, microphone and
echo regressions, keyboard accessibility, and small well-tested fixes are
especially useful. You do not have to be a Rust developer to contribute.
See [the near-term priorities](ROADMAP.md), [how to contribute](CONTRIBUTING.md),
and [who maintains it / how to get in touch](MAINTAINERS.md).

## What you can do

| Workflow | What VocalCode does |
| --- | --- |
| **Dictate into your apps** | Hold a shortcut, speak, and insert text into a supported focused field. Focus checks protect against accidental delivery to the wrong target. |
| **Take local meeting notes** | Record microphone and system audio, import audio, search transcripts, add bookmarks and speaker labels, and export notes. Recording always needs your confirmation and participants' permission. |
| **Teach your vocabulary** | Add dictionary entries or review a learned correction, edit it, or undo it. |
| **Recover your words** | History keeps recent dictations encrypted on this device (Off / 24 hours / 7 days); text that could not be typed stays there until you quit. Optional local diagnostics have their own storage controls. |
| **Choose your trade-offs** | Select local language/model routes, the non-speech noise filter, and experimental pause-delimited progressive typing. No GPU is required. |

This is a desktop tool, not a meeting bot or a cloud transcription subscription.

### Defaults for a new install

| Setting | New install | Where to change it |
| --- | --- | --- |
| Mouse Back button (X1) sends Enter | Off. First run offers it as an unticked box, **Use the mouse Back button as Enter**. | Shortcuts → Tap to send |
| Filter non-speech noise | On. Without it, fan, keyboard or pink noise can be typed as "I.", "그." or "我。". | Settings → Dictation |
| Keep history | 7 days: up to 50 recent dictations, encrypted with your Windows account or a Keychain key, then deleted. Off deletes what was kept. | History |

Updating does not change these for an existing install. Settings written by
1.4.0 or earlier keep the Back button as Enter if they had it, keep the noise
filter off and keep History to the current session, until you change them.

The current **development tree** also includes an opt-in Windows desktop control
bar and a manual rewrite scratchpad. The bar is off by default and uses the
existing dictation engine; rewriting defaults to an already-installed local
Ollama model, with a separately consented Claude CLI option. These changes have
not been published as a new release by this work. See the
[development evaluation](docs/product-polish-2026-09-23/RESULTS.zh-CN.md) and
[rewrite boundaries](docs/SMART-REWRITE.md).

## Install or build

Open [Releases](https://github.com/wudaming00/vocalcode-community/releases) and choose
a stable `v*` release: `VocalCodeCommunitySetup.exe` for Windows x64 or
`VocalCodeCommunity-<version>.dmg` for Apple-silicon macOS. Each includes matching
source and SHA-256 checksums. On macOS, drag **VocalCode Community.app** to Applications.

Install the native prerequisites in [BUILDING.md](BUILDING.md), then:

```sh
git clone https://github.com/wudaming00/vocalcode-community.git
cd vocalcode-community
cargo build -p vocalcode-app --release --locked --features community
```

No payment account, activation code, or operator credential is required.
This repository enables `community` by default; the explicit flag
makes the intended edition clear.

The community edition has its own installation, data directory, instance identity,
autostart entry, and update channel. It does not migrate or remove legacy data.
Close the other edition before use to avoid competing global input hooks.
Dictionary/snippet import is explicit; back up data before any manual migration.

## Privacy and network access

- Speech recognition and meeting audio processing run on the device after
  the selected model is downloaded. Recognition audio and transcripts are
  not uploaded by these pipelines.
- First-time model and native-runtime downloads use the network. Sources,
  sizes, and hashes are pinned where supported by the manifest/build system.
- Community mode does not provision a trial, refresh a paid licence, or open
  checkout. It checks GitHub Releases for community updates, never the paid
  release channel; installation verifies size, SHA-256, publisher and edition.
- External calendar URLs and other explicitly configured integrations have
  their own network behavior. Do not confuse local ASR with an air-gapped app.
- The development [rewrite scratchpad](docs/SMART-REWRITE.md) defaults to local
  Ollama. Optionally choosing Claude Code passes the manually supplied
  source to that CLI, with fresh consent per preview. Its configured service,
  account policies, and logging remain separate trust boundaries.
  Installing a CLI locally does not make its inference local. Codex is
  detected only, not used for generation.
- Recording, clipboard history, exports, kept History and optional diagnostic
  persistence can contain sensitive information. Kept History is encrypted and
  expires, and **Remove…** under Settings → System deletes it with the rest of
  the app data. Review your OS sync and backup settings.

## Languages and performance

Different languages use different local model routes, including Parakeet,
SenseVoice, and Paraformer; experimental routes are not a blanket quality
promise. Model capability is not the same as native-speaker product validation.
Latency depends on CPU, model size, utterance length, and capture setup.

See [third-party notices](THIRD-PARTY-NOTICES.txt) and
[the model integrity manifest](packaging/models.json). Model weights have
their own licences, independent of the client source licence.

## Current limitations

- Windows x64 is the local validation environment. GitHub Actions also builds
  on Apple silicon; a successful hosted build does not replace native-device
  audio, permissions, and installation tests before a binary release.
- Overlapping speakers and loudspeaker echo can still damage transcription.
  The meeting echo guard is not a perfect speech-separation system.
- Progressive typing is experimental: it inserts stable segments, not a
  continuously rewritten live hypothesis.
- Meeting detection is advisory, not proof that a call has started or ended.
- Linux packaging is not provided. Legacy paid builds do not automatically
  switch to this community channel.

## Project layout

```text
vocalcode-app/       Desktop UI, workflows, model management, community policy
vocalcode-core/      Portable engine, configuration, and text processing
vocalcode-platform/ Native audio, focus checks, and input delivery
vocalcode-meeting/  Meeting storage, segmentation, echo handling, and export
vendor/             Required patched native bindings with upstream licences
```

## Contributing and licensing

Start with [the contribution guide](CONTRIBUTING.md) and
[security policy](SECURITY.md). The remaining release decisions are explicit
in [PUBLICATION_BLOCKERS.md](PUBLICATION_BLOCKERS.md).

VocalCode's first-party desktop source is **AGPL-3.0-only**. Commercial use is
allowed under its terms; this is not a noncommercial licence. Redistribution
and modified network services carry corresponding-source obligations. Your
ordinary recordings and transcripts do not become public by using the app.
See [LICENSE](LICENSE), [licensing scope](LICENSING.md), and
[brand guidance](BRANDING.md). Models and other third-party components retain
their own licences.

Community support is best-effort, with no promised response time, fix date,
or service-level agreement. Existing purchases are a separate matter; this
source preparation does not announce a change to their terms. Never put
private recordings, credentials, or security exploits in a public issue.

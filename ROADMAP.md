# Where help will make a difference

This is a set of priorities, not a release-date promise or a claim that these
items are already finished. Prefer a focused issue and regression test over
adding a new feature before the existing workflow is dependable.

## Release infrastructure and remaining validation

- Keep dependency audits green; the initial source preview upgraded Rustls
  to 0.23.45 for RUSTSEC-2026-0285. Remaining dependency warnings need follow-up.
- Audit the exact exported files for secrets, private data, ownership, model
  terms, and required notices; strengthen the export allowlist/scanner.
- VocalCode now installs with the paid releases' identity and data folder, so
  a paid installation updates in place; the early free builds' data imports
  from Settings. Keep validating different-version upgrades on real devices,
  and add the macOS equivalent of the Windows end-to-end update check.
- Keep the free-build labels and meeting/correction UI regression tests
  consistent with the activation-free product and the public CI pipeline.
- Validate clean-user Windows and native Apple-silicon workflows, then prepare
  real synthetic-content screenshots and an honest build/download guide.
- Maintain the issue/PR/private-security endpoints, and pair any distributed
  binary with its exact Corresponding Source.

The detailed publication gate is [PUBLICATION_BLOCKERS.md](PUBLICATION_BLOCKERS.md).

## Reliability work to contribute

| Area | A useful contribution | Evidence to include |
| --- | --- | --- |
| Short answers and background noise | A minimal synthetic speech/noise regression | Reference text, capture/model settings, expected keep/reject behavior |
| Meeting echo and double-talk | Reproducible separate near/far audio fixtures | Timing, channel mapping, intended transcript, permission/provenance |
| Focus and text delivery | A regression for Win32, Electron, or WebView2 | Exact app/version, reproduction, no private input content |
| Learning and UI | Keyboard-accessible review, undo, and meeting prompts | Steps, expected state, a test, synthetic screenshots |
| Language/model quality | Native-speaker-reviewed examples and CPU benchmarks | Audio duration, hardware, model revision, cold/warm timings and reference text |

## Non-goals for this stage

No silent recording, hidden audio uploads, fabricated benchmark claims,
guaranteed perfect transcription, or promised 24/7 support. No hosted service
or enterprise tier is being announced by this source release preparation.

Start with [CONTRIBUTING.md](CONTRIBUTING.md). You can help with documentation,
language review, and reproductions without writing Rust. See
[MAINTAINERS.md](MAINTAINERS.md) for who maintains the project and how contact
works for the public repository.

# Contributing

VocalCode is maintained by [Daming Wu](MAINTAINERS.md). Start with a small
reproducible [issue](https://github.com/wudaming00/vocalcode-community/issues)
or focused [pull request](https://github.com/wudaming00/vocalcode-community/pulls).
See [ROADMAP.md](ROADMAP.md) for useful starting points.

## Contribution licence

By intentionally submitting a first-party contribution for inclusion in this
project, you agree to license it under **AGPL-3.0-only**, as described in
[LICENSE](LICENSE) and [LICENSING.md](LICENSING.md). You retain copyright.
Only contribute material you have the right to submit. Preserve the notices
on third-party material and explain its origin and licence separately.

There is no copyright assignment, separate commercial re-licensing grant,
CLA, or required DCO sign-off in this contribution policy. A later change
to contributor terms would need explicit notice; this is not a promise that
the maintainer can relicense other contributors' work unilaterally.

## Ways to help without writing Rust

- Reproduce an issue with synthetic audio and exact device/model information.
- Review recognition examples in a language you speak; include reference text.
- Test keyboard navigation, readable layouts, and installation documentation.
- Improve translations or explain an unclear privacy/recording control.

Never feel obliged to donate, buy a licence, or provide a private recording to
participate. Forks are permitted by AGPL; contributing upstream is welcome,
not mandatory. Support is best-effort, without a guaranteed response time.

## Work that matters most

- Reliable first-word capture, short answers, and multilingual code-switching.
- Safe focused-field delivery, especially Win32, Electron, and WebView2.
- Meeting echo/double-talk quality, stop/recovery behavior, and accessibility.
- Reproducible CPU benchmarks and native-speaker-reviewed language fixtures.

## A useful bug report

Include app version/edition, OS, CPU, selected model, capture device, exact
steps, expected behavior, and actual behavior. Redact emails, document paths,
meeting URLs, transcripts, licence keys, and other personal information.
Attach synthetic audio where possible. Never upload recordings of others
without their permission, or credentials from a diagnostic log.

## Before proposing a change

1. Follow [BUILDING.md](BUILDING.md) and reproduce the issue.
2. Keep the fix narrowly scoped; preserve focus safety, cancellation, data
   recovery, and the separation between capture and text injection.
3. Add a regression test and run formatting, workspace tests, and Clippy.
4. Explain model/native-library provenance changes and update notices.
5. Describe tests that need hardware, permissions, or opt-in fixtures separately.

Do not claim language accuracy or a latency improvement from a single demo.
Do not add background audio uploads, silent recording, or hidden telemetry.
Report security issues privately as described in [SECURITY.md](SECURITY.md).

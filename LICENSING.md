# VocalCode licensing

## First-party desktop source: AGPL-3.0-only

Copyright (c) 2026 VocalCode contributors.

VocalCode's first-party desktop source is licensed under the **GNU Affero
General Public License, version 3 only** (`AGPL-3.0-only`). You may use,
modify, and redistribute the covered work under the terms of [LICENSE](LICENSE).
No option to choose a later version is granted by this notice. The software
is provided without warranty, as described in sections 15 and 16 of LICENSE.

This grant covers first-party material in `vocalcode-app/`, `vocalcode-core/`,
`vocalcode-platform/`, and `vocalcode-meeting/`, and the first-party build
configuration, community tooling, documentation, and artwork supplied with
the community source snapshot, except material carrying a separate notice.
It applies independently of whether the `community` build feature is enabled.
The feature controls product behavior, not the source-code licence.

This is not a blanket grant over everything in the original private working
directory. Unexported payment/delivery backends, private operational tools,
website/marketing material, customer records, credentials, recordings,
diagnostics, and private Git history are not included in this grant. Their
presence next to the desktop workspace does not make them public source.
The public receipt-contract fixture included for desktop tests is first-party
test data, not the payment service or a production credential.

## What this means

This summary is explanatory; the unmodified licence text controls.

- Commercial use and charging for copies are allowed when the licence is met.
  This is not a noncommercial licence or a requirement to pay the maintainer.
- Redistributing a covered executable requires providing its Corresponding
  Source in a way allowed by section 6, including required build/install files.
  A link to an unrelated or older upstream release is not a substitute for
  the source corresponding to a modified binary.
- If you modify the program and users interact with that version remotely
  over a computer network, section 13 requires a prominent offer of access
  to that version's Corresponding Source to those users.
- Private local modifications do not by themselves require publication.
  The licence does not require sending a pull request to this project.
- Your ordinary dictation, meeting recordings, and transcripts do not become
  AGPL-licensed merely because you use VocalCode. They are not contributions
  to this repository. Do not upload private content in a bug report.

## Third-party software and models

The grant above does not replace third-party copyright notices or licences.
See [THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt),
`THIRD-PARTY-LICENSES/`, and the licences accompanying vendored source.
Dependencies and upstream-derived patches retain their applicable notices.
Model weights are separately licensed; selecting AGPL for the desktop code
does not relicense SenseVoice, Parakeet, Paraformer, Qwen, or other models.
The bundled Silero VAD detector retains its MIT notice.

Do not describe the entire download/model ecosystem as uniformly AGPL.
Compatibility, attribution, exact model provenance, and redistribution of
native runtime libraries must be reviewed for each release artifact.

## Brand, contributions, and release status

See [BRANDING.md](BRANDING.md) for accurate attribution and avoiding confusion
with official builds. It does not add a noncommercial restriction to AGPL.
For contributions, follow the public snapshot's `CONTRIBUTING.md`: contributors
retain copyright and license their first-party contributions under the same
AGPL-3.0-only terms. No copyright assignment or separate commercial licence
is implied.

The owner approved this licence choice on 2026-09-22. That decision does not
certify every file's ownership, clear the publication audit, publish a
repository, or promise support for an unreleased installer. Release checklists
are the maintainer's workflow, not additional restrictions on AGPL rights.
This preparation does not change deployed billing or installed applications.

## Licence text provenance

`LICENSE` is the unmodified AGPL v3 text obtained from the
[SPDX licence list](https://github.com/spdx/license-list-data/blob/main/text/AGPL-3.0-only.txt).
SHA-256 of the retrieved UTF-8 text (LF line endings):
`d8a6cc31abc16b6748c7a21f21611f5a1ec33f67d22ca23d7da1c19b95496bee`.

Authoritative references:

- [GNU AGPL v3](https://www.gnu.org/licenses/agpl-3.0.html)
- [OSI licence text](https://opensource.org/license/agpl-3.0)

The explanatory files do not modify the terms of LICENSE.

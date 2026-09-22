# VocalCode sherpa-onnx-sys patch

This directory vendors the FFI surface from the official
`sherpa-onnx-sys` 1.13.6 crate published by k2-fsa under Apache-2.0. Its build
path retains VocalCode's previously reviewed bounded download, extraction, and
cache hardening.

The upstream crate selects TTS-enabled sherpa-onnx desktop archives. Those
archives include piper/espeak-ng code that VocalCode does not use and cannot
distribute in its proprietary executable. This patch instead pins the official
sherpa-onnx v1.13.6 shared no-TTS release assets and their SHA-256 checksums:

- Windows x64: `sherpa-onnx-v1.13.6-win-x64-shared-MD-Release-no-tts.tar.bz2`
  (`071d6641efd737a1f60de48c9c4cd596f78d5b0980815e8ad3798c95785d2b26`)
- macOS universal2: `sherpa-onnx-v1.13.6-osx-universal2-shared-no-tts.tar.bz2`
  (`a16e7b91784775616c3b92f2a4947de3ac340a1df9fb9b610d86447f0857b150`)
- Linux x64: `sherpa-onnx-v1.13.6-linux-x64-shared-no-tts.tar.bz2`
  (`089c01bded9166ed53f9e433b030863f9dd5ec2ad067dca73a8ba96dca25979d`)
- Linux arm64: `sherpa-onnx-v1.13.6-linux-aarch64-shared-no-tts.tar.bz2`
  (`3a11e89b5a12cd1f8809e385087df7194e8463ab240cf530e5d7cc6d7b27fae7`)

The build refuses desktop overrides, static assets, non-no-TTS names, and the
upstream unsafe checksum bypass. Platform packaging separately rejects any
actual library with embedded `espeak_`, `espeak_ng_`, `phonemize_eSpeak`, or
`piper` symbols. (The official no-TTS library retains inert OfflineTts API
stubs, so those public API names are not evidence that espeak is present.)

Archive downloads also require a strictly numeric, nonzero `Content-Length`
no greater than 512 MiB. The response is streamed through a declared-size + 1
byte limiter and must exactly match the declared length before the pinned
SHA-256 checksum is verified.

Persistent extracted libraries are never trusted or linked. A build-local
cached compressed archive is bounded and re-hashed on every build, then
extracted into a fresh build-local directory. Extraction rejects
absolute/traversing or duplicate
paths, links and special entries, more than 20,000 entries, and expansion beyond
2 GiB. Windows extraction additionally rejects case aliases, trailing dots or
spaces, alternate data streams, reserved device names, forbidden characters,
and non-ASCII components.

Build-source preparation is a bounded pure-Rust copy that rejects links,
reparse points and special files, skips `scripts` only in the destination, and
never modifies the vendored source. Completed or failed extraction generations
carry a private marker and are cleaned only after a 24-hour linker-safety
retention window plus containment and plain-tree validation. Recent generations
remain isolated so concurrent Cargo build-script instances cannot delete one
another's linker inputs; the container still has a hard entry limit. The
linker handoff retains Cargo's ordinary absolute path rather than the Windows
`canonicalize()` `\\?\` form, and uses short private directory names so MSVC
receives an import-library path comfortably below its legacy path limit.

Upstream also leaves any same-named runtime already in `target/<profile>`
untouched. This patch replaces those exact files when the build script runs,
so a persistent release runner cannot accidentally package an older archive.

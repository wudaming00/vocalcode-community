# Bundled acoustic detector

`silero-v5.onnx` is the unchanged Silero VAD v5.0 ONNX artifact from
<https://raw.githubusercontent.com/snakers4/silero-vad/v5.0/files/silero_vad.onnx>.

- Size: 2,313,101 bytes.
- SHA-256: `6b99cbfd39246b6706f98ec13c7c50c6b299181f2474fa05cbc8046acc274396`.
- MIT licence and attribution: `../../THIRD-PARTY-LICENSES/Silero-VAD-LICENSE.txt`.

It is embedded in the signed application, then atomically extracted to the
trusted data directory on first enabled, non-progressive dictation. Extraction
never overwrites an existing file. The runtime verifies the size and hash
before loading; failures preserve the normal recognition path. Disabled
filtering neither extracts nor loads it. ASR models remain separate downloads.

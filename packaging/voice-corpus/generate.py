"""Render packaging/voice-corpus/cases.json into spoken test clips.

Every clip is synthetic: a test sentence written for this corpus, spoken by a
generic TTS voice. No person's recording, dictation history or voice clone is
used. Audio is never committed (default output: target/voice-corpus, which
is ignored) because provider terms for redistributing generated audio differ;
the manifest and this generator are the reproducible, committed part.

    python packaging/voice-corpus/generate.py [--out DIR] [--voices voices.json]
        [--only-provider edge|sapi|fish] [--limit N]

Providers:
  edge  Microsoft neural voices through `python3 -m edge_tts` in WSL (network)
  sapi  Windows SAPI desktop voices (offline, robotic: a deliberately hard case)
  fish  Fish Audio through a credential-safe adapter you already trust:
        VOCALCODE_FISH_ADAPTER = directory whose `harness/` package provides
        tts_fish (FishTts, FishTtsRequest, VaultApiKeySource) and
        identityvault; VOCALCODE_FISH_STATE = its state directory holding
        fish-tts.json. The key stays inside that adapter; nothing here reads
        or prints it.

Variants (applied with ffmpeg after rendering a clean 16 kHz clip):
  clean  as rendered
  noisy  mixed with pink noise at ~15 dB SNR (a fan, an office)
  fast   1.15x tempo without pitch change (people speed up)
  quiet  -14 dB (a soft voice or a far microphone)
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import wave
from pathlib import Path

HERE = Path(__file__).resolve().parent
DEFAULT_OUT = HERE.parent.parent / "target" / "voice-corpus"


def ffmpeg() -> str:
    found = shutil.which("ffmpeg")
    if found:
        return found
    winget = Path(os.environ.get("LOCALAPPDATA", "")) / "Microsoft/WinGet/Packages"
    for candidate in winget.glob("Gyan.FFmpeg*/**/bin/ffmpeg.exe"):
        return str(candidate)
    raise SystemExit("ffmpeg not found")


FFMPEG = ffmpeg()


def to_pcm16(source: Path, target: Path, extra: list[str] | None = None) -> None:
    cmd = [FFMPEG, "-hide_banner", "-loglevel", "error", "-y", "-i", str(source)]
    cmd += extra or []
    cmd += ["-ac", "1", "-ar", "16000", "-c:a", "pcm_s16le", str(target)]
    subprocess.run(cmd, check=True)


def seconds(path: Path) -> float:
    with wave.open(str(path)) as reader:
        return reader.getnframes() / reader.getframerate()


def wsl_path(path: Path) -> str:
    drive, rest = str(path.resolve()).split(":", 1)
    return f"/mnt/{drive.lower()}{rest.replace(chr(92), '/')}"


def render_edge(voice: dict, text: str, target: Path) -> None:
    with tempfile.TemporaryDirectory(dir=target.parent) as tmp:
        tmp = Path(tmp)
        (tmp / "t.txt").write_text(text, encoding="utf-8")
        mp3 = tmp / "a.mp3"
        cmd = ["wsl", "-e", "python3", "-m", "edge_tts", "--voice", voice["voice"],
               "--file", wsl_path(tmp / "t.txt"), "--write-media", wsl_path(mp3)]
        if voice.get("rate"):
            cmd.insert(cmd.index("--file"), f"--rate={voice['rate']}")
        for attempt in range(4):
            result = subprocess.run(cmd, capture_output=True, text=True)
            if result.returncode == 0 and mp3.exists() and mp3.stat().st_size > 1000:
                break
            time.sleep(2 + 3 * attempt)
        else:
            raise RuntimeError(f"edge-tts failed for {voice['voice']}: {result.stderr[-300:]}")
        to_pcm16(mp3, target)


def render_sapi(voice: dict, text: str, target: Path) -> None:
    with tempfile.TemporaryDirectory(dir=target.parent) as tmp:
        tmp = Path(tmp)
        (tmp / "t.txt").write_text(text, encoding="utf-8")
        raw = tmp / "a.wav"
        script = (
            "Add-Type -AssemblyName System.Speech;"
            "$s=New-Object System.Speech.Synthesis.SpeechSynthesizer;"
            f"$s.SelectVoice('{voice['voice']}');$s.Rate={int(voice.get('rate', 0))};"
            f"$s.SetOutputToWaveFile('{raw}');"
            f"$s.Speak([IO.File]::ReadAllText('{tmp / 't.txt'}',[Text.Encoding]::UTF8));$s.Dispose()"
        )
        subprocess.run(["powershell", "-NoProfile", "-Command", script], check=True, capture_output=True)
        to_pcm16(raw, target)


_FISH = None


def render_fish(voice: dict, text: str, target: Path) -> None:
    global _FISH
    if _FISH is None:
        adapter = os.environ.get("VOCALCODE_FISH_ADAPTER")
        state_dir = os.environ.get("VOCALCODE_FISH_STATE")
        if not adapter or not state_dir:
            raise RuntimeError("set VOCALCODE_FISH_ADAPTER and VOCALCODE_FISH_STATE to use Fish voices")
        sys.path.insert(0, adapter)
        from harness.identityvault import IdentityVault
        from harness.tts_fish import FishTts, FishTtsConfig, VaultApiKeySource

        state = Path(state_dir)
        config = json.loads((state / "fish-tts.json").read_text(encoding="utf-8-sig"))
        source = VaultApiKeySource(
            vault=IdentityVault(state_dir=state), ref=config["api_key_ref"],
            collie_id=config["collie_id"], account=config["vault_account"], kind=config["vault_kind"],
        )
        _FISH = FishTts(api_key=source, config=FishTtsConfig(model=voice.get("model", "s2.1-pro-free"), request_timeout_seconds=90))
    from harness.tts_fish import FishTtsRequest, FishTtsError

    for attempt in range(4):
        try:
            audio = _FISH.synthesize(FishTtsRequest(text=text, reference_id=voice.get("reference_id")))
            break
        except FishTtsError as error:  # credential-safe adapter messages only
            if attempt == 3:
                raise RuntimeError(f"fish failed: {type(error).__name__}: {error}") from None
            time.sleep(5 + 10 * attempt)
    with tempfile.TemporaryDirectory(dir=target.parent) as tmp:
        raw = Path(tmp) / "a.wav"
        raw.write_bytes(audio.data)
        to_pcm16(raw, target)


RENDER = {"edge": render_edge, "sapi": render_sapi, "fish": render_fish}


def make_noise(out: Path) -> Path:
    noise = out / "_noise_pink.wav"
    if not noise.exists():
        subprocess.run([FFMPEG, "-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i",
                        "anoisesrc=color=pink:amplitude=1:duration=120:seed=7", "-ac", "1", "-ar", "16000",
                        "-c:a", "pcm_s16le", str(noise)], check=True)
    return noise


def variant(clean: Path, kind: str, target: Path, noise: Path) -> None:
    if kind == "fast":
        to_pcm16(clean, target, ["-filter:a", "atempo=1.15"])
    elif kind == "quiet":
        to_pcm16(clean, target, ["-filter:a", "volume=-14dB"])
    elif kind == "noisy":
        # Speech normalised to -20 dBFS-ish loudness, pink noise ~15 dB below.
        subprocess.run([FFMPEG, "-hide_banner", "-loglevel", "error", "-y", "-i", str(clean), "-i", str(noise),
                        "-filter_complex",
                        "[0:a]loudnorm=I=-23:TP=-2[s];[1:a]volume=0.035[n];[s][n]amix=inputs=2:duration=first:normalize=0",
                        "-ac", "1", "-ar", "16000", "-c:a", "pcm_s16le", str(target)], check=True)
    else:
        raise ValueError(kind)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", type=Path, default=DEFAULT_OUT)
    parser.add_argument("--voices", type=Path, default=HERE / "voices.json")
    parser.add_argument("--only-provider")
    parser.add_argument("--limit", type=int)
    args = parser.parse_args()
    cases = json.loads((HERE / "cases.json").read_text(encoding="utf-8"))["cases"]
    voices = json.loads(args.voices.read_text(encoding="utf-8"))["voices"]
    out: Path = args.out
    (out / "audio").mkdir(parents=True, exist_ok=True)
    noise = make_noise(out)
    clips_path = out / "clips.json"
    clips = {c["path"]: c for c in json.loads(clips_path.read_text(encoding="utf-8"))} if clips_path.exists() else {}
    done = 0
    for voice in voices:
        if args.only_provider and voice["provider"] != args.only_provider:
            continue
        for case in cases:
            if case["language"] != voice["language"]:
                continue
            if args.limit and done >= args.limit:
                break
            digest = hashlib.sha256(json.dumps([voice, case["say"]], ensure_ascii=False).encode()).hexdigest()[:10]
            folder = out / "audio" / voice["id"]
            folder.mkdir(parents=True, exist_ok=True)
            clean = folder / f"{case['id']}.clean.{digest}.wav"
            if not clean.exists():
                started = time.monotonic()
                try:
                    RENDER[voice["provider"]](voice, case["say"], clean)
                except Exception as error:  # keep going; record the failure
                    print(json.dumps({"voice": voice["id"], "case": case["id"], "error": str(error)[:300]}), flush=True)
                    continue
                print(json.dumps({"voice": voice["id"], "case": case["id"], "seconds": round(seconds(clean), 2),
                                  "render_s": round(time.monotonic() - started, 1)}, ensure_ascii=False), flush=True)
            kinds = ["clean"] + list(voice.get("variants", []))
            for kind in kinds:
                path = clean if kind == "clean" else folder / f"{case['id']}.{kind}.{digest}.wav"
                if kind != "clean" and not path.exists():
                    variant(clean, kind, path, noise)
                rel = str(path.relative_to(out)).replace("\\", "/")
                clips[rel] = {"path": rel, "case": case["id"], "language": case["language"], "voice": voice["id"],
                              "provider": voice["provider"], "variant": kind, "seconds": round(seconds(path), 3)}
            done += 1
            clips_path.write_text(json.dumps(sorted(clips.values(), key=lambda c: c["path"]), ensure_ascii=False, indent=1),
                                  encoding="utf-8")
    print(json.dumps({"clips": len(clips), "out": str(out)}))


if __name__ == "__main__":
    main()

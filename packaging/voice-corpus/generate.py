"""Render packaging/voice-corpus/cases.json into spoken test clips.

Every clip is synthetic: a test sentence written for this corpus, spoken by a
generic TTS voice. No person's recording, dictation history or voice clone is
used. Audio is never committed (default output: target/voice-corpus, which
is ignored) because provider terms for redistributing generated audio differ;
the manifest and this generator are the reproducible, committed part.

    python packaging/voice-corpus/generate.py [--out DIR] [--voices voices.json]
        [--only-provider edge|sapi|fish] [--only CASE,CASE] [--limit N]

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

Two kinds of case are not one sentence per voice:
  no_speech  1-3 s of synthesized fan hum, keyboard clicks, cafe babble
             (time-reversed synthetic voices, so no words survive), pink noise
             or near-silent room tone at a stated RMS level. One clip each,
             language "any": every route replays it.
  long_form  the first N sentences of a script in `scripts`, spoken by one
             voice, silence-trimmed and joined so no pause reaches the app's
             240 ms pause boundary: the whole dictation is one decode. The
             "pink46" variant adds pink noise at -46 dBFS RMS, which also
             keeps every pause above the boundary's quiet threshold.
Both are built with the standard library from rendered or synthesized PCM.
"""
from __future__ import annotations

import argparse
import array
import hashlib
import json
import math
import os
import random
import shutil
import subprocess
import sys
import tempfile
import time
import wave
from pathlib import Path

HERE = Path(__file__).resolve().parent
DEFAULT_OUT = HERE.parent.parent / "target" / "voice-corpus"
RATE = 16_000
FRAME = RATE // 100  # the app's 10 ms pause-detection frame

# segmentation::pause_boundary treats a 10 ms frame as quiet at an RMS of at
# most min(8% of the phrase peak, 0.003) and cuts after 24 quiet frames. Long-
# form clips keep every run of frames under this ceiling shorter than 18
# frames, with margin for the app's own frame alignment.
PAUSE_QUIET_RMS = 0.003
LONG_MAX_QUIET_FRAMES = 18
LONG_JOIN_MS = 150


_FFMPEG: str | None = None


def ffmpeg() -> str:
    global _FFMPEG
    if _FFMPEG:
        return _FFMPEG
    found = shutil.which("ffmpeg")
    if not found:
        winget = Path(os.environ.get("LOCALAPPDATA", "")) / "Microsoft/WinGet/Packages"
        found = next((str(c) for c in winget.glob("Gyan.FFmpeg*/**/bin/ffmpeg.exe")), None)
    if not found:
        raise SystemExit("ffmpeg not found")
    _FFMPEG = found
    return found


def to_pcm16(source: Path, target: Path, extra: list[str] | None = None) -> None:
    cmd = [ffmpeg(), "-hide_banner", "-loglevel", "error", "-y", "-i", str(source)]
    cmd += extra or []
    cmd += ["-ac", "1", "-ar", "16000", "-c:a", "pcm_s16le", str(target)]
    subprocess.run(cmd, check=True)


def seconds(path: Path) -> float:
    with wave.open(str(path)) as reader:
        return reader.getnframes() / reader.getframerate()


def read_pcm(path: Path) -> list[float]:
    with wave.open(str(path)) as reader:
        if (reader.getnchannels(), reader.getsampwidth(), reader.getframerate()) != (1, 2, RATE):
            raise ValueError(f"{path}: expected 16 kHz mono PCM16")
        data = array.array("h", reader.readframes(reader.getnframes()))
    if sys.byteorder == "big":
        data.byteswap()
    return [v / 32768.0 for v in data]


def write_pcm(samples: list[float], target: Path) -> None:
    data = array.array("h", (max(-32768, min(32767, round(v * 32768.0))) for v in samples))
    if sys.byteorder == "big":
        data.byteswap()
    partial = target.with_name(target.name + ".partial")
    with wave.open(str(partial), "wb") as writer:
        writer.setnchannels(1)
        writer.setsampwidth(2)
        writer.setframerate(RATE)
        writer.writeframes(data.tobytes())
    os.replace(partial, target)


def rms(samples: list[float]) -> float:
    return math.sqrt(sum(v * v for v in samples) / max(1, len(samples)))


def dbfs(level: float) -> float:
    return 20 * math.log10(max(level, 1e-12))


def scaled(samples: list[float], rms_dbfs: float) -> list[float]:
    """Samples scaled to an RMS level in dB relative to digital full scale (1.0)."""
    gain = 10 ** (rms_dbfs / 20) / max(rms(samples), 1e-12)
    return [v * gain for v in samples]


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
        subprocess.run([ffmpeg(), "-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i",
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
        subprocess.run([ffmpeg(), "-hide_banner", "-loglevel", "error", "-y", "-i", str(clean), "-i", str(noise),
                        "-filter_complex",
                        "[0:a]loudnorm=I=-23:TP=-2[s];[1:a]volume=0.035[n];[s][n]amix=inputs=2:duration=first:normalize=0",
                        "-ac", "1", "-ar", "16000", "-c:a", "pcm_s16le", str(target)], check=True)
    else:
        raise ValueError(kind)


def sentence_clip(voice: dict, text: str, folder: Path, stem: str) -> Path:
    """Render `text` with `voice` once; cached by voice + sentence hash."""
    digest = hashlib.sha256(json.dumps([voice, text], ensure_ascii=False).encode()).hexdigest()[:10]
    folder.mkdir(parents=True, exist_ok=True)
    clean = folder / f"{stem}.clean.{digest}.wav"
    if not clean.exists():
        started = time.monotonic()
        RENDER[voice["provider"]](voice, text, clean)
        print(json.dumps({"voice": voice["id"], "case": stem, "seconds": round(seconds(clean), 2),
                          "render_s": round(time.monotonic() - started, 1)}, ensure_ascii=False), flush=True)
    return clean


# ------------------------------------------------------------ no-speech audio


def pink(rng: random.Random, count: int) -> list[float]:
    """Paul Kellet's pink-noise filter over Gaussian white noise."""
    b0 = b1 = b2 = b3 = b4 = b5 = b6 = 0.0
    out = []
    for _ in range(count):
        w = rng.gauss(0.0, 1.0)
        b0 = 0.99886 * b0 + w * 0.0555179
        b1 = 0.99332 * b1 + w * 0.0750759
        b2 = 0.96900 * b2 + w * 0.1538520
        b3 = 0.86650 * b3 + w * 0.3104856
        b4 = 0.55000 * b4 + w * 0.5329522
        b5 = -0.7616 * b5 - w * 0.0168980
        out.append(b0 + b1 + b2 + b3 + b4 + b5 + b6 + w * 0.5362)
        b6 = w * 0.115926
    return out


def fan(rng: random.Random, count: int) -> list[float]:
    """Broadband airflow rumble plus a slowly wobbling blade-pass hum."""
    air, low = pink(rng, count), 0.0
    blade = rng.uniform(80.0, 110.0)
    phase = [rng.uniform(0, 2 * math.pi) for _ in range(3)]
    out = []
    for i, a in enumerate(air):
        low = 0.985 * low + 0.015 * rng.gauss(0.0, 1.0) * 6
        t = i / RATE
        wobble = 1 + 0.15 * math.sin(2 * math.pi * 0.6 * t)
        hum = sum(math.sin(2 * math.pi * blade * (k + 1) * t + phase[k]) / (k + 1.5) for k in range(3))
        out.append(0.6 * a + low + 0.35 * wobble * hum)
    return out


def keys(rng: random.Random, count: int) -> list[float]:
    """Typing: a press and a release click per key, bursts with short breaks."""
    out = [0.02 * v for v in pink(rng, count)]
    t = rng.uniform(0.05, 0.2)
    while t < count / RATE - 0.02:
        for moment in (t, t + rng.uniform(0.06, 0.11)):
            start = int(moment * RATE)
            ring = rng.uniform(1800.0, 4200.0)
            level = rng.uniform(0.5, 1.0)
            for n in range(min(int(0.02 * RATE), count - start)):
                decay = math.exp(-n / (0.0015 * RATE))
                tone = math.exp(-n / (0.006 * RATE)) * math.sin(2 * math.pi * ring * n / RATE)
                out[start + n] += level * (decay * rng.gauss(0.0, 1.0) + 0.6 * tone)
        t += rng.uniform(0.09, 0.22) if rng.random() > 0.12 else rng.uniform(0.35, 0.7)
    # High-pass (first difference) takes the click body out of the rumble band.
    return [out[0]] + [out[i] - 0.9 * out[i - 1] for i in range(1, count)]


def babble(rng: random.Random, count: int, talkers: list[list[float]]) -> list[float]:
    """Cafe babble: several synthetic voices, time-reversed so no word is
    intelligible, overlapped from random starting points, over a pink bed."""
    out = [0.08 * v for v in pink(rng, count)]
    for voice in talkers:
        voice = [v for v in reversed(voice)] or [0.0]
        voice = scaled(voice, -20 + rng.uniform(-4, 2))
        offset = rng.randrange(len(voice))
        for i in range(count):
            out[i] += voice[(offset + i) % len(voice)]
    return out


def synthesize_noise(spec: dict, talkers: list[list[float]] | None = None) -> list[float]:
    rng = random.Random(spec["seed"])
    count = int(spec["seconds"] * RATE)
    kind = spec["kind"]
    if kind == "pink":
        raw = pink(rng, count)
    elif kind == "room":
        raw = [p + 0.3 * rng.gauss(0.0, 1.0) for p in pink(rng, count)]
    elif kind == "fan":
        raw = fan(rng, count)
    elif kind == "keys":
        raw = keys(rng, count)
    elif kind == "babble":
        raw = babble(rng, count, talkers or [])
    else:
        raise ValueError(f"unknown noise kind {kind!r}")
    # Remove DC, then 10 ms fades so the clip itself does not start with a click.
    mean = sum(raw) / max(1, count)
    raw = [v - mean for v in raw]
    fade = min(FRAME, count // 2)
    for i in range(fade):
        raw[i] *= i / fade
        raw[-1 - i] *= i / fade
    return scaled(raw, spec["rms_dbfs"])


# ------------------------------------------------------------ long-form audio


def frame_rms(samples: list[float], start: int) -> float:
    frame = samples[start:start + FRAME]
    mean = sum(frame) / len(frame)
    return math.sqrt(sum((v - mean) ** 2 for v in frame) / len(frame))


def trim(samples: list[float], margin_frames: int = 3) -> list[float]:
    """Drop leading and trailing quiet frames, keeping a short margin."""
    loud = [i for i in range(0, len(samples) - FRAME + 1, FRAME) if frame_rms(samples, i) > PAUSE_QUIET_RMS]
    if not loud:
        return []
    start = max(0, loud[0] - margin_frames * FRAME)
    end = min(len(samples), loud[-1] + FRAME + margin_frames * FRAME)
    return samples[start:end]


def cap_pauses(samples: list[float], max_frames: int = LONG_MAX_QUIET_FRAMES) -> list[float]:
    """Shorten every run of quiet 10 ms frames to at most `max_frames` by
    removing its middle, so the result has no pause the app would cut at."""
    out: list[float] = []
    run: list[list[float]] = []

    def flush() -> None:
        if len(run) > max_frames:
            keep = run[:max_frames // 2] + run[len(run) - (max_frames - max_frames // 2):]
        else:
            keep = run
        for frame in keep:
            out.extend(frame)
        run.clear()

    whole = len(samples) - len(samples) % FRAME
    for start in range(0, whole, FRAME):
        frame = samples[start:start + FRAME]
        if frame_rms(samples, start) <= PAUSE_QUIET_RMS:
            run.append(frame)
        else:
            flush()
            out.extend(frame)
    flush()
    out.extend(samples[whole:])
    return out


def longest_quiet_run(samples: list[float]) -> int:
    longest = current = 0
    for start in range(0, len(samples) - FRAME + 1, FRAME):
        current = current + 1 if frame_rms(samples, start) <= PAUSE_QUIET_RMS else 0
        longest = max(longest, current)
    return longest


def join_sentences(parts: list[list[float]]) -> list[float]:
    gap = [0.0] * (RATE * LONG_JOIN_MS // 1000)
    joined: list[float] = []
    for n, part in enumerate(parts):
        if n:
            joined.extend(gap)
        joined.extend(trim(part))
    return cap_pauses(joined)


# ------------------------------------------------------------ manifest


def save_clips(clips_path: Path, clips: dict) -> None:
    """Merge into clips.json atomically. Entries already on disk that this run
    did not produce (another generator, a hand-made subset) are kept."""
    merged = {}
    if clips_path.exists():
        merged = {c["path"]: c for c in json.loads(clips_path.read_text(encoding="utf-8"))}
    merged.update(clips)
    partial = clips_path.with_name(clips_path.name + ".partial")
    partial.write_text(json.dumps(sorted(merged.values(), key=lambda c: c["path"]), ensure_ascii=False, indent=1),
                       encoding="utf-8")
    os.replace(partial, clips_path)


def no_speech_clip(case: dict, cases: list[dict], voices: dict, out: Path) -> Path:
    spec = case["noise"]
    talkers, sources = [], []
    for source in spec.get("sources", []):
        voice_id, case_id = source.split(":", 1)
        said = next(c["say"] for c in cases if c["id"] == case_id)
        clip = sentence_clip(voices[voice_id], said, out / "audio" / voice_id, case_id)
        sources.append(clip.name)
        talkers.append(read_pcm(clip))
    digest = hashlib.sha256(json.dumps([spec, sources]).encode()).hexdigest()[:10]
    path = out / "audio" / "_no-speech" / f"{case['id']}.{spec['kind']}.{digest}.wav"
    path.parent.mkdir(parents=True, exist_ok=True)
    if not path.exists():
        samples = synthesize_noise(spec, talkers)
        write_pcm(samples, path)
        print(json.dumps({"case": case["id"], "seconds": round(seconds(path), 2),
                          "rms_dbfs": round(dbfs(rms(samples)), 1),
                          "peak_dbfs": round(dbfs(max(abs(v) for v in samples)), 1)}), flush=True)
    return path


def long_form_clips(case: dict, scripts: dict, voice: dict, out: Path, noise: Path) -> list[tuple[str, Path]]:
    sentences = scripts[case["script"]][:case["sentences"]]
    parts = [sentence_clip(voice, text, out / "audio" / voice["id"] / "_long", f"{case['script']}-{n:02}")
             for n, text in enumerate(sentences)]
    digest = hashlib.sha256(json.dumps([p.name for p in parts]).encode()).hexdigest()[:10]
    folder = out / "audio" / "_long-form"
    folder.mkdir(parents=True, exist_ok=True)
    clean = folder / f"{case['id']}.clean.{digest}.wav"
    if not clean.exists():
        joined = join_sentences([read_pcm(p) for p in parts])
        write_pcm(joined, clean)
        print(json.dumps({"case": case["id"], "seconds": round(len(joined) / RATE, 2),
                          "longest_quiet_ms": 10 * longest_quiet_run(joined)}), flush=True)
    made = [("clean", clean)]
    for kind in case.get("variants", []):
        if kind != "pink46":
            raise ValueError(f"unknown long-form variant {kind!r}")
        path = folder / f"{case['id']}.{kind}.{digest}.wav"
        if not path.exists():
            speech, bed = read_pcm(clean), read_pcm(noise)
            bed = scaled([bed[i % len(bed)] for i in range(len(speech))], -46)
            write_pcm([s + n for s, n in zip(speech, bed)], path)
        made.append((kind, path))
    return made


def wanted(case_id: str, only: list[str]) -> bool:
    return not only or any(part in case_id for part in only)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", type=Path, default=DEFAULT_OUT)
    parser.add_argument("--voices", type=Path, default=HERE / "voices.json")
    parser.add_argument("--only-provider")
    parser.add_argument("--only", default="", help="comma-separated case-id substrings")
    parser.add_argument("--limit", type=int)
    args = parser.parse_args()
    manifest = json.loads((HERE / "cases.json").read_text(encoding="utf-8"))
    cases, scripts = manifest["cases"], manifest.get("scripts", {})
    voices = json.loads(args.voices.read_text(encoding="utf-8"))["voices"]
    only = [part for part in args.only.split(",") if part]
    out: Path = args.out
    (out / "audio").mkdir(parents=True, exist_ok=True)
    noise = make_noise(out)
    clips_path = out / "clips.json"
    clips = {c["path"]: c for c in json.loads(clips_path.read_text(encoding="utf-8"))} if clips_path.exists() else {}
    fresh: dict = {}

    def record(path: Path, case: dict, voice: dict | None, kind: str) -> None:
        rel = str(path.relative_to(out)).replace("\\", "/")
        fresh[rel] = clips[rel] = {
            "path": rel, "case": case["id"], "language": case["language"],
            "voice": voice["id"] if voice else "synthetic", "provider": voice["provider"] if voice else "synthetic",
            "variant": kind, "seconds": round(seconds(path), 3)}

    done = 0
    for voice in voices:
        if args.only_provider and voice["provider"] != args.only_provider:
            continue
        for case in cases:
            if case["language"] != voice["language"] or "say" not in case or not wanted(case["id"], only):
                continue
            if args.limit and done >= args.limit:
                break
            folder = out / "audio" / voice["id"]
            try:
                clean = sentence_clip(voice, case["say"], folder, case["id"])
            except Exception as error:  # keep going; record the failure
                print(json.dumps({"voice": voice["id"], "case": case["id"], "error": str(error)[:300]}), flush=True)
                continue
            digest = clean.name.split(".")[-2]
            for kind in ["clean"] + list(voice.get("variants", [])):
                path = clean if kind == "clean" else folder / f"{case['id']}.{kind}.{digest}.wav"
                if kind != "clean" and not path.exists():
                    variant(clean, kind, path, noise)
                record(path, case, voice, kind)
            done += 1
            save_clips(clips_path, fresh)

    by_id = {v["id"]: v for v in voices}
    for case in cases:
        if case.get("feature") != "no_speech" or not wanted(case["id"], only):
            continue
        try:
            path = no_speech_clip(case, cases, by_id, out)
        except Exception as error:  # keep going; record the failure
            print(json.dumps({"case": case["id"], "error": str(error)[:300]}), flush=True)
            continue
        record(path, case, None, "clean")
    save_clips(clips_path, fresh)

    for case in cases:
        if case.get("feature") != "long_form" or not wanted(case["id"], only):
            continue
        voice = by_id[case["voice"]]
        if args.only_provider and voice["provider"] != args.only_provider:
            continue
        try:
            made = long_form_clips(case, scripts, voice, out, noise)
        except Exception as error:  # keep going; record the failure
            print(json.dumps({"case": case["id"], "error": str(error)[:300]}), flush=True)
            continue
        for kind, path in made:
            record(path, case, voice, kind)
        save_clips(clips_path, fresh)
    print(json.dumps({"clips": len(clips), "out": str(out)}))


if __name__ == "__main__":
    main()

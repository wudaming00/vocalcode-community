"""One command for the voice-corpus release test.

    python packaging/voice-corpus/run.py [--corpus DIR] [--models DIR] [--routes R] [--skip-generate]

1. generate.py renders any missing clips (cached by voice + sentence hash);
2. runs the app's ignored `voice_corpus` test: every clip through the
   production pipeline (models::build_asr, the route's cleaner chain, the
   inference worker with the speech gate, the built-in dictionary, snippets,
   the Engine's per-utterance settings), twice (all rules on / all rules off),
   on every route in --routes;
3. score.py checks every expectation and the release gates.

Exit status is score.py's: 0 only when all gates hold. Reports land next to
the corpus as results.jsonl, report.json and summary.md.
"""
from __future__ import annotations

import argparse
import os
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
ROUTES = "zh:sensevoice,en:sensevoice,en:qwen3-asr-0.6b,en:parakeet-tdt-v3"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--corpus", type=Path, default=Path(r"C:\workspace\vocalcode-voice-corpus"))
    parser.add_argument("--models", type=Path, default=Path(r"C:\workspace\vocalcode-qa-models"),
                        help="directory with one sub-directory per model id (sha256-verified copies)")
    parser.add_argument("--routes", default=ROUTES)
    parser.add_argument("--skip-generate", action="store_true")
    args = parser.parse_args()
    if not args.skip_generate:
        subprocess.run([sys.executable, str(HERE / "generate.py"), "--out", str(args.corpus)], check=True)
    results = args.corpus / "results.jsonl"
    env = dict(os.environ, VOCALCODE_QA_MODELS=str(args.models), VOCALCODE_VOICE_CORPUS=str(args.corpus),
               VOCALCODE_VOICE_RESULTS=str(results), VOCALCODE_VOICE_ROUTES=args.routes,
               VOCALCODE_VOICE_CASES=str(HERE / "cases.json"))
    subprocess.run(["cargo", "test", "--release", "--locked", "-p", "vocalcode-app", "voice_corpus", "--",
                    "--ignored", "--nocapture", "--test-threads=1"], cwd=ROOT, env=env, check=True)
    return subprocess.run([sys.executable, str(HERE / "score.py"), str(results),
                           "--report", str(args.corpus / "report.json"), "--md", str(args.corpus / "summary.md")]).returncode


if __name__ == "__main__":
    sys.exit(main())

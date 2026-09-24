"""Score voice-corpus results (JSONL from the app's `voice_corpus` test) against
packaging/voice-corpus/cases.json.

    python packaging/voice-corpus/score.py <results.jsonl> [--report report.json] [--md summary.md]

Exit status 0 only when every release gate holds on every route:

  G1 guards      no rule fires on a sentence that merely talks about it, on any
                 voice or variant, with every rule enabled
  G2 baseline    with every rule off nothing is sent and plain/guard text is
                 byte-identical to the all-on pass (rules only act when asked)
  G3 rules       when the recogniser heard the command words, the rule fired:
                 rule misses (attributed below) are at most 2% of feature clips
  G4 features    on clean neural voices (edge, fish) features succeed on >= 85%
                 of clips per route, whatever the recogniser did

Every failure is attributed: `rule_miss` means the raw recognition contained
the spoken command yet the output is wrong (our bug); `asr_miss` means the
recogniser did not produce the command words (a model limit, reported, and
only gated through G4). WER/CER of plain dictation is reported per route.
"""
from __future__ import annotations

import argparse
import json
import re
import sys
import unicodedata
from collections import defaultdict
from pathlib import Path

HERE = Path(__file__).resolve().parent
NEURAL = {"edge", "fish"}

# Words the recogniser must produce for a rule to be able to fire.
TRIGGERS = {
    "commands": [["new line", "newline"], ["new paragraph"], ["换行"], ["另起一段"]],
    "backtrack": [["scratch that", "strike that"], ["删掉上一句"]],
    "press_enter": [["press enter"], ["回车"]],
    "code": [["camel case", "camelcase", "caml case"], ["snake case", "snakecase"], ["open paren", "openparen"],
             ["close paren", "closeparen"], ["question mark"]],
    "lists": [["first", "number one"], ["second", "number two"], ["third", "number three"], ["第一"], ["第二"], ["第三"]],
    # The snippet's name is part of the command: "Snippet Sign." is a miss by
    # the recogniser, not by the expansion.
    "snippet": [["snippet"], ["插入词块"], ["signature"], ["签名"]],
    "fillers": [["um", "uh"], ["呃"]],
}


def norm_units(text: str, language: str) -> list[str]:
    text = unicodedata.normalize("NFKC", text).lower().replace("’", "'")
    if language == "zh":
        return [c for c in text if c.isalnum()]
    text = re.sub(r"(?<=\w)'(?=\w)", "", text)
    return re.findall(r"\w+", text)


def edit_distance(a: list[str], b: list[str]) -> int:
    previous = list(range(len(b) + 1))
    for i, x in enumerate(a, 1):
        current = [i]
        for j, y in enumerate(b, 1):
            current.append(min(previous[j] + 1, current[j - 1] + 1, previous[j - 1] + (x != y)))
        previous = current
    return previous[-1]


def error_rate(reference: str, hypothesis: str, language: str) -> float:
    ref = norm_units(reference, language)
    return edit_distance(ref, norm_units(hypothesis, language)) / max(1, len(ref))


def flat(text: str) -> str:
    """Lower-case words separated by single spaces, punctuation removed."""
    text = unicodedata.normalize("NFKC", text).lower()
    return " " + " ".join(re.findall(r"[\w']+", text)) + " "


def has(text: str, needle: str, language: str) -> bool:
    if language == "zh" and not needle.isascii():
        return needle in text
    return needle.lower() in text.lower()


def heard_triggers(case: dict, heard: str) -> bool:
    """Did the raw recognition contain every command the case speaks?"""
    groups = TRIGGERS.get(case["feature"], [])
    said = flat(case["say"])
    got = flat(heard)
    for group in groups:
        spoken = [w for w in group if f" {w} " in said or w in case["say"]]
        if spoken and not any(f" {w} " in got or (not w.isascii() and w in heard) for w in group):
            return False
    return True


def check(expect: dict, record: dict, language: str) -> list[str]:
    typed: str = record["typed"]
    problems = []
    if "newlines" in expect and typed.count("\n") != expect["newlines"]:
        problems.append(f"newlines {typed.count(chr(10))} != {expect['newlines']}")
    if "blank_lines" in expect and typed.count("\n\n") != expect["blank_lines"]:
        problems.append(f"blank lines {typed.count(chr(10) * 2)} != {expect['blank_lines']}")
    if "list_items" in expect:
        items = len(re.findall(r"(?m)^\d+\. ", typed))
        if items != expect["list_items"]:
            problems.append(f"list items {items} != {expect['list_items']}")
    for needle in expect.get("present", []):
        if not has(typed, needle, language):
            problems.append(f"missing {needle!r}")
    for group in expect.get("present_any", []):
        if not any(has(typed, n, language) for n in group):
            problems.append(f"missing any of {group}")
    for needle in expect.get("absent", []):
        if has(typed, needle, language):
            problems.append(f"still has {needle!r}")
    for group in expect.get("absent_any", []):
        if any(has(typed, n, language) for n in group):
            problems.append(f"still has one of {group}")
    for word in expect.get("absent_words", []):
        if re.search(rf"(?i)\b{re.escape(word)}\b", typed):
            problems.append(f"still has word {word!r}")
    if "send" in expect and record["send"] != expect["send"]:
        problems.append(f"send {record['send']} != {expect['send']}")
    if "ends_with" in expect and not typed.rstrip().endswith(expect["ends_with"]):
        problems.append(f"does not end with {expect['ends_with']!r}")
    if "not_ends_with" in expect and typed.rstrip().endswith(expect["not_ends_with"]):
        problems.append(f"ends with {expect['not_ends_with']!r}")
    if expect.get("starts_lower"):
        first = next((c for c in typed if c.isalpha()), "")
        if not first.islower():
            problems.append("does not start lower-case")
    if "equals" in expect and typed != expect["equals"]:
        problems.append(f"not equal to {expect['equals']!r}")
    return problems


def ratio(passed: int, total: int) -> float:
    return passed / total if total else 1.0


def pct(pair) -> str:
    passed, total = pair
    return f"{passed}/{total} ({100 * ratio(passed, total):.0f}%)"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("results", type=Path)
    parser.add_argument("--report", type=Path)
    parser.add_argument("--md", type=Path)
    args = parser.parse_args()
    cases = {c["id"]: c for c in json.loads((HERE / "cases.json").read_text(encoding="utf-8"))["cases"]}
    rows = []
    for line in args.results.read_text(encoding="utf-8").splitlines():
        try:
            rows.append(json.loads(line))
        except json.JSONDecodeError:
            pass  # a run still being written ends with a partial line
    by_clip: dict[tuple, dict] = defaultdict(dict)
    for row in rows:
        by_clip[(row.get("route", "?"), row["path"])][row["pass"]] = row

    per_route = defaultdict(lambda: {
        "features_all": defaultdict(lambda: [0, 0]), "features_clean_neural": defaultdict(lambda: [0, 0]),
        "by_voice": defaultdict(lambda: [0, 0]), "by_variant": defaultdict(lambda: [0, 0]),
        "plain": defaultdict(list), "rule_miss": 0, "asr_miss": 0, "feature_clips": 0,
    })
    guard_failures, baseline_failures, feature_failures = [], [], []
    for (route, path), passes in sorted(by_clip.items()):
        stats = per_route[route]
        on, base = passes.get("all_on"), passes.get("baseline")
        if not on or not base:
            baseline_failures.append({"route": route, "path": path, "problem": "missing a pass"})
            continue
        case = cases[on["case"]]
        language, feature = case["language"], case["feature"]
        if base["send"]:
            baseline_failures.append({"route": route, "path": path, "problem": "baseline pressed Enter", "typed": base["typed"]})
        if feature == "plain":
            if on["typed"] != base["typed"]:
                baseline_failures.append({"route": route, "path": path, "problem": "rules changed plain text",
                                          "all_on": on["typed"], "baseline": base["typed"]})
            stats["plain"][f"{on['provider']} {on['variant']}"].append(error_rate(case["reference"], base["typed"], language))
            continue
        problems = check(case.get("expect", {}), on, language)
        if feature == "guard":
            # A guard fails only when a rule acted on it. The recogniser
            # mis-hearing the guard sentence itself is not a false trigger.
            if on["typed"] != base["typed"] or on["send"]:
                guard_failures.append({"route": route, "path": path, "case": case["id"],
                                       "problems": problems or ["rules changed guard text"],
                                       "heard": on["heard"], "all_on": on["typed"], "baseline": base["typed"]})
            continue
        ok = not problems
        stats["feature_clips"] += 1
        for bucket in (stats["features_all"][feature], stats["by_voice"][on["voice"]], stats["by_variant"][on["variant"]]):
            bucket[0] += ok
            bucket[1] += 1
        if on["provider"] in NEURAL and on["variant"] == "clean":
            stats["features_clean_neural"][feature][0] += ok
            stats["features_clean_neural"][feature][1] += 1
        if not ok:
            # Only a missing word is a recognition problem; a missing symbol,
            # a wrong line/item count, a leftover command or a wrong Enter is
            # structure the rules own.
            structural = [p for p in problems
                          if not (p.startswith("missing ") and any(ch.isalnum() for ch in p[8:]))]
            # A recognition that is mostly wrong words ("提纲" for "Sounds
            # good. See you at five.") is the recogniser's miss even where the
            # feature has no command words to look for.
            recognised = error_rate(case["say"], on["heard"], case["language"]) <= 0.5
            cause = ("rule_miss" if structural and recognised and heard_triggers(case, on["heard"])
                     else "asr_miss")
            stats[cause] += 1
            feature_failures.append({"route": route, "cause": cause, "case": case["id"], "feature": feature,
                                     "voice": on["voice"], "variant": on["variant"], "problems": problems,
                                     "heard": on["heard"], "typed": on["typed"], "path": path})

    gates = {"G1 guards": not guard_failures, "G2 baseline": not baseline_failures}
    routes_report = {}
    for route, stats in sorted(per_route.items()):
        clean = [sum(v[0] for v in stats["features_clean_neural"].values()),
                 sum(v[1] for v in stats["features_clean_neural"].values())]
        gates[f"G3 rules {route}"] = stats["rule_miss"] <= 0.02 * max(1, stats["feature_clips"])
        gates[f"G4 features {route}"] = ratio(*clean) >= 0.85
        routes_report[route] = {
            "rule_miss": stats["rule_miss"], "asr_miss": stats["asr_miss"], "feature_clips": stats["feature_clips"],
            "features_clean_neural": {k: pct(v) for k, v in sorted(stats["features_clean_neural"].items())},
            "features_clean_neural_total": pct(clean),
            "features_all": {k: pct(v) for k, v in sorted(stats["features_all"].items())},
            "by_variant": {k: pct(v) for k, v in sorted(stats["by_variant"].items())},
            "by_voice": {k: pct(v) for k, v in sorted(stats["by_voice"].items())},
            "plain_error_rate_percent": {k: round(100 * sum(v) / len(v), 2) for k, v in sorted(stats["plain"].items())},
        }
    report = {"clips": len(by_clip), "gates": gates, "routes": routes_report, "guard_failures": guard_failures,
              "baseline_failures": baseline_failures, "feature_failures": feature_failures}
    if args.report:
        args.report.write_text(json.dumps(report, ensure_ascii=False, indent=1), encoding="utf-8")
    lines = [f"# Voice corpus: {len(by_clip)} route x clip replays", ""]
    lines += [f"- {'PASS' if v else 'FAIL'} {k}" for k, v in gates.items()]
    for route, r in routes_report.items():
        lines += ["", f"## {route}", f"- features on clean neural voices: {r['features_clean_neural_total']}",
                  f"- failures: rule_miss {r['rule_miss']}, asr_miss {r['asr_miss']} of {r['feature_clips']} feature clips"]
        lines += [f"  - {k}: clean {r['features_clean_neural'].get(k, '-')} | all {v}" for k, v in r["features_all"].items()]
        lines += ["- by variant: " + ", ".join(f"{k} {v}" for k, v in r["by_variant"].items())]
        lines += ["- plain WER/CER %: " + ", ".join(f"{k} {v}" for k, v in r["plain_error_rate_percent"].items())]
    lines += ["", f"Guard failures: {len(guard_failures)} | Baseline failures: {len(baseline_failures)} | "
              f"Feature failures: {len(feature_failures)} "
              f"(rule_miss {sum(1 for f in feature_failures if f['cause'] == 'rule_miss')})"]
    summary = "\n".join(lines)
    if args.md:
        args.md.write_text(summary, encoding="utf-8")
    print(summary)
    return 0 if all(gates.values()) else 1


if __name__ == "__main__":
    sys.exit(main())

"""Score voice-corpus results (JSONL from the app's `voice_corpus` test) against
packaging/voice-corpus/cases.json.

    python packaging/voice-corpus/score.py <results.jsonl> [--report report.json] [--md summary.md]

Exit status 0 only when every release gate holds on every route:

  G1 guards      no rule fires on a sentence that merely talks about it, on any
                 voice or variant, with every rule enabled
  G2 baseline    with every rule off nothing is sent, and plain, code-switched
                 and guard text is byte-identical to the all-on pass (rules
                 only act when asked)
  G3 rules       when the recogniser heard the command words, the rule fired:
                 rule misses (attributed below) are at most 2% of feature
                 clips, code-switched ones included
  G4 features    on clean neural voices (edge, fish) features succeed on >= 85%
                 of clips per route, whatever the recogniser did. Code-switched
                 clips (cases with English `terms`) are reported beside it, not
                 in it: whether a recogniser hears "npm" in Chinese speech is a
                 model limit, and our part of those clips is gated by G3
  G5 no-speech   fan hum, keyboard clicks, pink noise and room tone type
                 nothing and press nothing with the speech gate on (the
                 default since 1.4.1), on every route; every no-speech case
                 must have been replayed in both gate states. Gate-off
                 replays and babble (voices the speech detector rightly calls
                 speech) are reported beside the gate, not in it
  G6 long-form   30/60/90 s dictations with no pause the app can cut at (and
                 with a -46 dBFS pink-noise floor) come back whole: non-empty,
                 WER/CER against the joined script within the case's bound,
                 never the bare word "language"; every long-form case of the
                 route's language must have been replayed

Every feature failure is attributed: `rule_miss` means the raw recognition
contained the spoken command (and, for Chinese punctuation, the full-width
mark) yet the output is wrong: our text pipeline's bug; `asr_miss` means the
recogniser did not produce the words (a model limit, reported, and only gated
through G4). Reported per route: WER/CER of plain dictation, code-switch term
recall (English terms inside Chinese dictation, case-insensitive), full-width
punctuation in Chinese lines, and the mixed error rate (one unit per Chinese
character or English word) of code-switched dictation.
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
# Features with no command words: the rules must leave their text untouched.
RULE_FREE = {"plain", "code_switch"}
CJK = "\u3400-\u9fff\uf900-\ufaff"

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
        # Mixed error rate: one unit per Chinese character, one per ASCII
        # word or number, so "npm run build" weighs three units, not eleven.
        return re.findall(r"[a-z0-9]+|[^\W_a-z0-9]", text)
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


def has_term(text: str, term: str) -> bool:
    """An English term, whole, in any case, however it is spaced: "user ID"
    matches "user id" and "userID", "roadmap" matches "road map", but "PR"
    does not match "PRD"."""
    body = r"\s*".join(re.escape(char) for char in term if not char.isspace())
    return re.search(rf"(?<![a-z0-9]){body}(?![a-z0-9])", unicodedata.normalize("NFKC", text), re.I) is not None


def half_width_in_chinese(text: str) -> list[str]:
    """Half-width , . ! ? ; : used as punctuation in a line containing Chinese.

    Chinese text takes full-width marks even right after an English term
    ("提交一个新的 PR。"). A mark between two ASCII letters or digits is part of
    a token ("index.html", "1.5", "3:30") and a leading "1. " is a list
    number, so neither counts."""
    found = []
    for line in text.split("\n"):
        if not re.search(f"[{CJK}]", line):
            continue
        body = re.sub(r"^\s*\d+\.\s", "", line)
        for mark in re.finditer(r"[,.!?;:]", body):
            i = mark.start()
            before, after = body[i - 1:i], body[i + 1:i + 2]
            if before.isascii() and before.isalnum() and after.isascii() and after.isalnum():
                continue
            found.append(body[max(0, i - 8):i + 1])
    return found


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


def check(expect: dict, record: dict, language: str, reference: str | None = None) -> list[str]:
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
    for term in expect.get("terms", []):
        if not has_term(typed, term):
            problems.append(f"missing term {term!r}")
    for needle in expect.get("absent", []):
        if has(typed, needle, language):
            problems.append(f"still has {needle!r}")
    for group in expect.get("absent_any", []):
        if any(has(typed, n, language) for n in group):
            problems.append(f"still has one of {group}")
    for word in expect.get("absent_words", []):
        # ASCII boundaries, so a word glued to Chinese ("的language") counts.
        if re.search(rf"(?i)(?<![a-z0-9]){re.escape(word)}(?![a-z0-9])", typed):
            problems.append(f"still has word {word!r}")
    if expect.get("cjk_punct"):
        marks = half_width_in_chinese(typed)
        if marks:
            problems.append(f"half-width punctuation in Chinese: {marks}")
    if "max_error_rate" in expect and reference is not None:
        rate = error_rate(reference, typed, language)
        if rate > expect["max_error_rate"]:
            problems.append(f"error rate {100 * rate:.1f}% > {100 * expect['max_error_rate']:.0f}%")
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


def long_form_reference(case: dict, scripts: dict) -> str:
    sentences = scripts[case["script"]][:case["sentences"]]
    return ("" if case["language"] == "zh" else " ").join(sentences)


def ratio(passed: int, total: int) -> float:
    return passed / total if total else 1.0


def pct(pair) -> str:
    passed, total = pair
    return f"{passed}/{total} ({100 * ratio(passed, total):.0f}%)"


def mean_pct(values: list[float]) -> float:
    return round(100 * sum(values) / len(values), 2)


def new_route_stats() -> dict:
    return {
        "features_all": defaultdict(lambda: [0, 0]), "features_clean_neural": defaultdict(lambda: [0, 0]),
        "by_voice": defaultdict(lambda: [0, 0]), "by_variant": defaultdict(lambda: [0, 0]),
        "plain": defaultdict(list), "rule_miss": 0, "asr_miss": 0, "feature_clips": 0,
        "terms": [0, 0], "terms_clean_neural": [0, 0], "term_hits": defaultdict(lambda: [0, 0]),
        "cjk_punct": [0, 0], "cjk_punct_ours": 0, "code_switch_mer": defaultdict(list),
        "no_speech": defaultdict(lambda: [0, 0]), "gate_decisions": defaultdict(int), "no_speech_cases": set(),
        "long_form": [], "long_form_cases": set(),
    }


def score(rows: list[dict], manifest: dict) -> dict:
    cases = {c["id"]: dict(c) for c in manifest["cases"]}
    for case in cases.values():
        if case["feature"] == "long_form":
            case["reference"] = long_form_reference(case, manifest.get("scripts", {}))
    by_clip: dict[tuple, dict] = defaultdict(dict)
    for row in rows:
        by_clip[(row.get("route", "?"), row["path"])][row["pass"]] = row

    per_route = defaultdict(new_route_stats)
    guard_failures, baseline_failures, feature_failures = [], [], []
    no_speech_failures, long_form_failures = [], []
    for (route, path), passes in sorted(by_clip.items()):
        stats = per_route[route]
        on, base = passes.get("all_on"), passes.get("baseline")
        if not on or not base:
            baseline_failures.append({"route": route, "path": path, "problem": "missing a pass"})
            continue
        case = cases[on["case"]]
        language, feature = case["language"], case["feature"]
        expect = case.get("expect", {})
        if base["send"]:
            baseline_failures.append({"route": route, "path": path, "problem": "baseline pressed Enter", "typed": base["typed"]})

        if feature == "no_speech":
            stats["no_speech_cases"].add(case["id"])
            by_gate = defaultdict(list)
            for row in passes.values():
                # Results from before gate-off replays existed ran with the gate on.
                by_gate[row.get("speech_gate", "on")].append(row)
            for gate in ("on", "off"):
                if gate not in by_gate:
                    no_speech_failures.append({"route": route, "case": case["id"], "path": path,
                                               "problem": f"not replayed with the speech gate {gate}"})
            for gate, replays in sorted(by_gate.items()):
                silent = True
                for row in replays:
                    stats["gate_decisions"][f"gate {gate}: {row.get('gate', '?')}"] += 1
                    problems = check(expect, row, language)
                    if problems:
                        silent = False
                        # Gated: the shipped default on non-speech noise.
                        blocking = gate == "on" and case.get("noise", {}).get("kind") != "babble"
                        no_speech_failures.append({"route": route, "case": case["id"], "pass": row["pass"],
                                                   "speech_gate": gate, "decision": row.get("gate"),
                                                   "blocking": blocking,
                                                   "problems": problems, "heard": row["heard"],
                                                   "typed": row["typed"], "path": path})
                stats["no_speech"][gate][0] += silent
                stats["no_speech"][gate][1] += 1
            continue

        if feature == "long_form":
            stats["long_form_cases"].add(case["id"])
            entry = {"case": case["id"], "variant": on["variant"], "seconds": on.get("audio_seconds"),
                     "error_rate_percent": {}, "decode_s": {}, "asr_chunks": {}}
            for row in (on, base):
                name = row["pass"]
                problems = check(expect, row, language, case["reference"])
                if not row["typed"].strip():
                    problems.append("typed nothing")
                entry["error_rate_percent"][name] = round(100 * error_rate(case["reference"], row["typed"], language), 2)
                entry["decode_s"][name] = round(row.get("elapsed_ms", 0) / 1000, 1)
                entry["asr_chunks"][name] = row.get("asr_chunks")
                if problems:
                    long_form_failures.append({"route": route, "case": case["id"], "variant": row["variant"],
                                               "pass": name, "problems": problems,
                                               "typed_head": row["typed"][:160], "typed_tail": row["typed"][-160:],
                                               "path": path})
            stats["long_form"].append(entry)
            continue

        clean_neural = on["provider"] in NEURAL and on["variant"] == "clean"
        for term in expect.get("terms", []):
            found = has_term(on["typed"], term)
            for bucket in [stats["terms"], stats["term_hits"][term]] + ([stats["terms_clean_neural"]] if clean_neural else []):
                bucket[0] += found
                bucket[1] += 1
        if expect.get("cjk_punct"):
            fine = not half_width_in_chinese(on["typed"])
            stats["cjk_punct"][0] += fine
            stats["cjk_punct"][1] += 1
            stats["cjk_punct_ours"] += not fine and not half_width_in_chinese(on["heard"])

        if feature in RULE_FREE:
            if on["typed"] != base["typed"]:
                baseline_failures.append({"route": route, "path": path, "problem": "rules changed rule-free text",
                                          "all_on": on["typed"], "baseline": base["typed"]})
            rate = error_rate(case["reference"], base["typed"], language)
            if feature == "plain":
                stats["plain"][f"{on['provider']} {on['variant']}"].append(rate)
                continue
            stats["code_switch_mer"][f"{on['provider']} {on['variant']}"].append(rate)
        problems = check(expect, on, language)
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
        # Code-switched clips get their own buckets: how well a recogniser
        # hears English said inside Chinese is reported, not held to G4.
        name = feature if not expect.get("terms") else "code_switch" + ("" if feature == "code_switch" else f"+{feature}")
        for bucket in (stats["features_all"][name], stats["by_voice"][on["voice"]], stats["by_variant"][on["variant"]]):
            bucket[0] += ok
            bucket[1] += 1
        if clean_neural:
            stats["features_clean_neural"][name][0] += ok
            stats["features_clean_neural"][name][1] += 1
        if not ok:
            # Only a missing word is a recognition problem; a missing symbol,
            # a wrong line/item count, a leftover command or a wrong Enter is
            # structure the rules own.
            structural = [p for p in problems
                          if not (p.startswith("missing ") and any(ch.isalnum() for ch in p[8:]))]
            if half_width_in_chinese(on["heard"]):
                # The recogniser typed the half-width mark itself.
                structural = [p for p in structural if not p.startswith("half-width")]
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

    no_speech_ids = {c["id"] for c in cases.values() if c["feature"] == "no_speech"}
    gates = {"G1 guards": not guard_failures, "G2 baseline": not baseline_failures}
    routes_report = {}
    for route, stats in sorted(per_route.items()):
        language = route.split(":", 1)[0]
        clean, switched = [0, 0], [0, 0]
        for name, (passed, total) in stats["features_clean_neural"].items():
            target = switched if name.startswith("code_switch") else clean
            target[0] += passed
            target[1] += total
        gates[f"G3 rules {route}"] = stats["rule_miss"] <= 0.02 * max(1, stats["feature_clips"])
        gates[f"G4 features {route}"] = ratio(*clean) >= 0.85
        # Missing evidence fails a gate: a case nobody replayed did not pass.
        for case_id in sorted(no_speech_ids - stats["no_speech_cases"]):
            no_speech_failures.append({"route": route, "case": case_id, "problem": "not replayed"})
        gates[f"G5 no-speech {route}"] = not any(
            f["route"] == route and f.get("blocking", True) for f in no_speech_failures)
        long_ids = {c["id"] for c in cases.values() if c["feature"] == "long_form" and c["language"] == language}
        for case_id in sorted(long_ids - stats["long_form_cases"]):
            long_form_failures.append({"route": route, "case": case_id, "problems": ["not replayed"]})
        if long_ids:
            gates[f"G6 long-form {route}"] = not any(f["route"] == route for f in long_form_failures)
        routes_report[route] = {
            "rule_miss": stats["rule_miss"], "asr_miss": stats["asr_miss"], "feature_clips": stats["feature_clips"],
            "features_clean_neural": {k: pct(v) for k, v in sorted(stats["features_clean_neural"].items())},
            "features_clean_neural_total": pct(clean),
            "code_switch_features_clean_neural_total": pct(switched),
            "features_all": {k: pct(v) for k, v in sorted(stats["features_all"].items())},
            "by_variant": {k: pct(v) for k, v in sorted(stats["by_variant"].items())},
            "by_voice": {k: pct(v) for k, v in sorted(stats["by_voice"].items())},
            "plain_error_rate_percent": {k: mean_pct(v) for k, v in sorted(stats["plain"].items())},
            "code_switch": {
                "terms_checked": stats["terms"][1], "term_recall": pct(stats["terms"]), "term_recall_clean_neural": pct(stats["terms_clean_neural"]),
                "term_recall_by_term": {k: pct(v) for k, v in sorted(stats["term_hits"].items())},
                "full_width_punctuation": pct(stats["cjk_punct"]),
                "half_width_introduced_by_text_pipeline": stats["cjk_punct_ours"],
                "mixed_error_rate_percent": {k: mean_pct(v) for k, v in sorted(stats["code_switch_mer"].items())},
            },
            "no_speech": {
                "silent_by_gate": {f"gate {k}": pct(v) for k, v in sorted(stats["no_speech"].items())},
                "gate_decisions": dict(sorted(stats["gate_decisions"].items())),
            },
            "long_form": sorted(stats["long_form"], key=lambda e: (e["case"], e["variant"])),
        }
    return {"clips": len(by_clip), "gates": gates, "routes": routes_report, "guard_failures": guard_failures,
            "baseline_failures": baseline_failures, "feature_failures": feature_failures,
            "no_speech_failures": no_speech_failures, "long_form_failures": long_form_failures}


def summarize(report: dict) -> str:
    lines = [f"# Voice corpus: {report['clips']} route x clip replays", ""]
    lines += [f"- {'PASS' if v else 'FAIL'} {k}" for k, v in report["gates"].items()]
    for route, r in report["routes"].items():
        lines += ["", f"## {route}", f"- features on clean neural voices: {r['features_clean_neural_total']}",
                  f"- failures: rule_miss {r['rule_miss']}, asr_miss {r['asr_miss']} of {r['feature_clips']} feature clips"]
        lines += [f"  - {k}: clean {r['features_clean_neural'].get(k, '-')} | all {v}" for k, v in r["features_all"].items()]
        lines += ["- by variant: " + ", ".join(f"{k} {v}" for k, v in r["by_variant"].items())]
        lines += ["- plain WER/CER %: " + ", ".join(f"{k} {v}" for k, v in r["plain_error_rate_percent"].items())]
        cs = r["code_switch"]
        if cs["terms_checked"]:
            lines += [f"- code-switch: features on clean neural voices {r['code_switch_features_clean_neural_total']} "
                      f"(not in G4), term recall {cs['term_recall']} (clean neural {cs['term_recall_clean_neural']}), "
                      f"full-width punctuation {cs['full_width_punctuation']} "
                      f"({cs['half_width_introduced_by_text_pipeline']} made half-width by our text pipeline)"]
            if cs["mixed_error_rate_percent"]:
                lines += ["  - code-switch MER %: "
                          + ", ".join(f"{k} {v}" for k, v in cs["mixed_error_rate_percent"].items())]
        ns = r["no_speech"]
        if ns["silent_by_gate"]:
            lines += ["- no-speech silent: " + ", ".join(f"{k} {v}" for k, v in ns["silent_by_gate"].items())
                      + " | decisions: " + ", ".join(f"{k} {v}" for k, v in ns["gate_decisions"].items())]
        for entry in r["long_form"]:
            rates = entry["error_rate_percent"]
            lines += [f"- long-form {entry['case']} {entry['variant']} ({entry['seconds']} s): error % all_on "
                      f"{rates.get('all_on')} / baseline {rates.get('baseline')}, decode s {entry['decode_s'].get('all_on')}, "
                      f"chunks {entry['asr_chunks'].get('all_on')}"]
    failures = report["feature_failures"]
    lines += ["", f"Guard failures: {len(report['guard_failures'])} | Baseline failures: {len(report['baseline_failures'])} | "
              f"Feature failures: {len(failures)} (rule_miss {sum(1 for f in failures if f['cause'] == 'rule_miss')}) | "
              f"No-speech failures: {len(report['no_speech_failures'])} | "
              f"Long-form failures: {len(report['long_form_failures'])}"]
    for failure in report["no_speech_failures"]:
        what = failure.get("problem") or f"typed {failure['typed']!r} ({failure['pass']}, gate decision {failure['decision']})"
        lines += [f"- no-speech {failure['route']} {failure['case']}: {what}"]
    for failure in report["long_form_failures"]:
        lines += [f"- long-form {failure['route']} {failure['case']} {failure.get('variant', '')} "
                  f"{failure.get('pass', '')}: {'; '.join(failure['problems'])}"]
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("results", type=Path)
    parser.add_argument("--report", type=Path)
    parser.add_argument("--md", type=Path)
    parser.add_argument("--cases", type=Path, default=HERE / "cases.json")
    args = parser.parse_args()
    manifest = json.loads(args.cases.read_text(encoding="utf-8"))
    rows = []
    for line in args.results.read_text(encoding="utf-8").splitlines():
        try:
            rows.append(json.loads(line))
        except json.JSONDecodeError:
            pass  # a run still being written ends with a partial line
    report = score(rows, manifest)
    if args.report:
        args.report.write_text(json.dumps(report, ensure_ascii=False, indent=1), encoding="utf-8")
    summary = summarize(report)
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(errors="backslashreplace")  # Chinese text on a legacy code page
    if args.md:
        args.md.write_text(summary, encoding="utf-8")
    print(summary)
    return 0 if all(report["gates"].values()) else 1


if __name__ == "__main__":
    sys.exit(main())

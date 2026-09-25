"""Behaviour tests for the voice-corpus scorer and generator (no audio, no models).

    python -m unittest discover -s packaging/voice-corpus -p "test_*.py"
"""
from __future__ import annotations

import json
import math
import random
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import generate  # noqa: E402
import score  # noqa: E402

MANIFEST = {
    "cases": [
        {"id": "zh-cs-roadmap", "language": "zh", "feature": "code_switch", "say": "明天和 product manager 对一下 roadmap。",
         "reference": "明天和 product manager 对一下 roadmap。",
         "expect": {"terms": ["product manager", "roadmap"], "cjk_punct": True}},
        {"id": "ns-fan", "language": "any", "feature": "no_speech", "noise": {"kind": "fan", "seconds": 1.0, "rms_dbfs": -50, "seed": 1},
         "expect": {"equals": "", "send": False}},
        {"id": "ns-keys", "language": "any", "feature": "no_speech", "noise": {"kind": "keys", "seconds": 1.0, "rms_dbfs": -40, "seed": 2},
         "expect": {"equals": "", "send": False}},
        {"id": "en-long-30", "language": "en", "feature": "long_form", "voice": "v", "script": "en", "sentences": 2,
         "expect": {"max_error_rate": 0.1, "absent_words": ["language"]}},
        {"id": "zh-long-30", "language": "zh", "feature": "long_form", "voice": "v", "script": "zh", "sentences": 1,
         "expect": {"max_error_rate": 0.1, "absent_words": ["language"]}},
    ],
    "scripts": {"en": ["The build passed on every machine today.", "We will ship the release on Friday morning.", "Unused."],
                "zh": ["这周我们把支付服务迁到了新的集群。"]},
}


def row(route, path, case, pass_, typed, heard=None, gate="on", provider="edge", variant="clean", send=False):
    return {"route": route, "path": path, "case": case, "pass": pass_, "typed": typed, "heard": typed if heard is None else heard,
            "send": send, "voice": "v", "provider": provider, "variant": variant, "speech_gate": gate,
            "gate": "rejected" if gate == "on" else "off", "elapsed_ms": 1000, "asr_chunks": 1, "audio_seconds": 30.0}


def no_speech_rows(route, case, typed_off=""):
    path = f"audio/_no-speech/{case}.wav"
    return [row(route, path, case, "all_on", ""), row(route, path, case, "baseline", ""),
            row(route, path, case, "all_on+gate-off", typed_off, gate="off"),
            row(route, path, case, "baseline+gate-off", typed_off, gate="off")]


EN_LONG = "The build passed on every machine today. We will ship the release on Friday morning."


def long_rows(route, typed=EN_LONG):
    return [row(route, "audio/_long-form/en-long-30.wav", "en-long-30", p, typed) for p in ("all_on", "baseline")]


class TextChecks(unittest.TestCase):
    def test_terms_are_whole_case_insensitive_and_space_tolerant(self):
        self.assertTrue(score.has_term("这个API的response里少了一个userID字段。", "user ID"))
        self.assertTrue(score.has_term("少了一个 user id 字段", "user ID"))
        self.assertTrue(score.has_term("对一下road map。", "roadmap"))
        self.assertTrue(score.has_term("提交一个新的pr.", "PR"))
        self.assertTrue(score.has_term("跑一下NPM run build看看", "npm run build"))
        self.assertFalse(score.has_term("写一份PRD", "PR"))
        self.assertFalse(score.has_term("把这个bag修一下", "bug"))

    def test_half_width_marks_in_chinese_lines(self):
        self.assertEqual(score.half_width_in_chinese("明天和product manager对一下roadmap."), ["下roadmap."])
        self.assertTrue(score.half_width_in_chinese("这个PR, 我看过了。"))
        self.assertTrue(score.half_width_in_chinese("我们走吧."))
        # Full-width marks, tokens, list numbers and English-only lines are fine.
        for fine in ("明天和product manager对一下roadmap。", "版本1.5和index.html都在，3:30开会。",
                     "上线之前要做三件事：\n1. 跑一遍unit test\n2. 更新README", "Hi Sam, thanks.\n好的。"):
            self.assertEqual(score.half_width_in_chinese(fine), [], fine)

    def test_mixed_error_rate_counts_english_words_as_units(self):
        self.assertEqual(score.norm_units("跑一下 npm run build。", "zh"), ["跑", "一", "下", "npm", "run", "build"])
        self.assertEqual(score.error_rate("跑一下 npm run build", "跑一下npm run build", "zh"), 0)
        self.assertAlmostEqual(score.error_rate("跑一下 npm run build", "跑一下npm run built", "zh"), 1 / 6)

    def test_absent_words_catch_a_word_glued_to_chinese(self):
        record = {"typed": "总的来说language很好", "send": False}
        self.assertEqual(score.check({"absent_words": ["language"]}, record, "zh"), ["still has word 'language'"])
        record = {"typed": "Three languages ship today.", "send": False}
        self.assertEqual(score.check({"absent_words": ["language"]}, record, "en"), [])

    def test_error_rate_bound_needs_the_reference(self):
        record = {"typed": "The build passed.", "send": False}
        problems = score.check({"max_error_rate": 0.1}, record, "en", EN_LONG)
        self.assertEqual(len(problems), 1)
        self.assertTrue(problems[0].startswith("error rate"))


class Gates(unittest.TestCase):
    def gates(self, rows):
        return score.score(rows, MANIFEST)

    def test_no_speech_must_be_silent_in_both_gate_states(self):
        route = "en:sensevoice"
        report = self.gates(no_speech_rows(route, "ns-fan") + no_speech_rows(route, "ns-keys") + long_rows(route))
        self.assertTrue(report["gates"][f"G5 no-speech {route}"])
        self.assertTrue(report["gates"][f"G6 long-form {route}"])
        self.assertEqual(report["routes"][route]["no_speech"]["silent_by_gate"], {"gate off": "2/2 (100%)", "gate on": "2/2 (100%)"})

        report = self.gates(no_speech_rows(route, "ns-fan") + no_speech_rows(route, "ns-keys", typed_off="Yeah.") + long_rows(route))
        self.assertFalse(report["gates"][f"G5 no-speech {route}"])
        failure = report["no_speech_failures"][0]
        self.assertEqual((failure["case"], failure["speech_gate"], failure["typed"]), ("ns-keys", "off", "Yeah."))
        self.assertEqual(report["routes"][route]["no_speech"]["silent_by_gate"]["gate off"], "1/2 (50%)")

    def test_missing_gate_state_or_case_is_not_a_pass(self):
        route = "en:sensevoice"
        gate_on_only = [r for r in no_speech_rows(route, "ns-fan") if r["speech_gate"] == "on"]
        report = self.gates(gate_on_only + no_speech_rows(route, "ns-keys") + long_rows(route))
        self.assertFalse(report["gates"][f"G5 no-speech {route}"])
        self.assertIn("not replayed with the speech gate off", [f.get("problem") for f in report["no_speech_failures"]])

        report = self.gates(no_speech_rows(route, "ns-fan") + long_rows(route))
        self.assertFalse(report["gates"][f"G5 no-speech {route}"])
        self.assertEqual([(f["case"], f["problem"]) for f in report["no_speech_failures"]], [("ns-keys", "not replayed")])

    def test_long_form_bounds_the_error_rate_and_bans_the_bare_word(self):
        route = "en:qwen3-asr-0.6b"
        ns = no_speech_rows(route, "ns-fan") + no_speech_rows(route, "ns-keys")
        for typed, reason in (("language", "still has word 'language'"), ("", "typed nothing"),
                              ("The build passed on every machine today.", "error rate")):
            report = self.gates(ns + long_rows(route, typed))
            self.assertFalse(report["gates"][f"G6 long-form {route}"], typed)
            self.assertTrue(any(p.startswith(reason) for f in report["long_form_failures"] for p in f["problems"]), typed)
        # An English route owes no Chinese long-form evidence; a Chinese one does.
        report = self.gates(ns + long_rows(route))
        self.assertTrue(report["gates"][f"G6 long-form {route}"])
        report = self.gates(no_speech_rows("zh:sensevoice", "ns-fan"))
        self.assertFalse(report["gates"]["G6 long-form zh:sensevoice"])

    def test_code_switch_attribution_recall_and_rule_freedom(self):
        route, path = "zh:sensevoice", "audio/v/zh-cs-roadmap.clean.wav"
        # The recogniser wrote "roadmap。"; the half-width stop is ours.
        normalized = [row(route, path, "zh-cs-roadmap", p, "明天和product manager对一下roadmap.",
                          heard="明天和product manager对一下roadmap。") for p in ("all_on", "baseline")]
        report = self.gates(normalized)
        failure = report["feature_failures"][0]
        self.assertEqual(failure["cause"], "rule_miss")
        stats = report["routes"][route]
        self.assertEqual(stats["code_switch"]["term_recall"], "2/2 (100%)")
        self.assertEqual(stats["code_switch"]["full_width_punctuation"], "0/1 (0%)")
        self.assertEqual(stats["code_switch"]["half_width_introduced_by_text_pipeline"], 1)
        self.assertFalse(report["gates"][f"G3 rules {route}"])
        # Reported beside G4, not inside it.
        self.assertEqual(stats["code_switch_features_clean_neural_total"], "0/1 (0%)")
        self.assertEqual(stats["features_clean_neural_total"], "0/0 (100%)")
        self.assertTrue(report["gates"][f"G4 features {route}"])

        # A misheard term is the recogniser's miss, and the correct output passes.
        misheard = [row(route, path, "zh-cs-roadmap", p, "明天和product manager对一下road map。") for p in ("all_on", "baseline")]
        report = self.gates(misheard)
        self.assertEqual(report["feature_failures"], [])
        misheard = [row(route, path, "zh-cs-roadmap", p, "明天和product manager对一下肉麦。") for p in ("all_on", "baseline")]
        report = self.gates(misheard)
        self.assertEqual([f["cause"] for f in report["feature_failures"]], ["asr_miss"])
        self.assertEqual(report["routes"][route]["code_switch"]["term_recall"], "1/2 (50%)")

        # No command words were spoken: rules changing the text breaks G2.
        changed = [row(route, path, "zh-cs-roadmap", "all_on", "明天和product manager\n对一下roadmap。"),
                   row(route, path, "zh-cs-roadmap", "baseline", "明天和product manager对一下roadmap。")]
        self.assertFalse(self.gates(changed)["gates"]["G2 baseline"])


class Generator(unittest.TestCase):
    def tone(self, seconds, amplitude=0.1):
        return [amplitude * math.sin(2 * math.pi * 220 * i / generate.RATE) for i in range(int(seconds * generate.RATE))]

    def test_long_form_joins_without_a_pause_the_app_would_cut_at(self):
        silence = [0.0] * (generate.RATE // 2)
        sentence = silence + self.tone(1.0) + [0.0] * (generate.RATE * 2 // 5) + self.tone(1.0) + silence
        joined = generate.join_sentences([sentence, sentence, sentence])
        self.assertLessEqual(generate.longest_quiet_run(joined), generate.LONG_MAX_QUIET_FRAMES)
        self.assertLess(generate.LONG_MAX_QUIET_FRAMES, 24)  # segmentation::pause_boundary cuts at 24
        loud = sum(1 for start in range(0, len(joined) - generate.FRAME + 1, generate.FRAME)
                   if generate.frame_rms(joined, start) > generate.PAUSE_QUIET_RMS)
        self.assertGreaterEqual(loud, 6 * 100 - 6)  # all six seconds of tone survive

    def test_noise_is_deterministic_and_at_the_stated_level(self):
        talkers = [self.tone(0.7, 0.2), self.tone(1.3, 0.05)]
        for kind in ("pink", "room", "fan", "keys", "babble"):
            spec = {"kind": kind, "seconds": 1.2, "rms_dbfs": -44, "seed": 5}
            samples = generate.synthesize_noise(spec, talkers)
            self.assertEqual(len(samples), int(1.2 * generate.RATE), kind)
            self.assertAlmostEqual(generate.dbfs(generate.rms(samples)), -44, delta=0.3, msg=kind)
            self.assertLess(max(abs(v) for v in samples), 1.0, kind)
            self.assertEqual(samples, generate.synthesize_noise(spec, talkers), kind)
        with self.assertRaises(ValueError):
            generate.synthesize_noise({"kind": "applause", "seconds": 1, "rms_dbfs": -40, "seed": 1})

    def test_pcm_round_trip_and_manifest_merge(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            samples = [random.Random(3).uniform(-0.5, 0.5) for _ in range(1600)]
            generate.write_pcm(samples, tmp / "a.wav")
            back = generate.read_pcm(tmp / "a.wav")
            self.assertEqual(len(back), 1600)
            self.assertLess(max(abs(a - b) for a, b in zip(samples, back)), 1 / 32768 + 1e-9)
            self.assertAlmostEqual(generate.seconds(tmp / "a.wav"), 0.1)

            clips = tmp / "clips.json"
            clips.write_text(json.dumps([{"path": "audio/x/old.wav", "case": "old"}]), encoding="utf-8")
            generate.save_clips(clips, {"audio/_no-speech/ns.wav": {"path": "audio/_no-speech/ns.wav", "case": "ns"}})
            saved = json.loads(clips.read_text(encoding="utf-8"))
            self.assertEqual([c["path"] for c in saved], ["audio/_no-speech/ns.wav", "audio/x/old.wav"])
            self.assertEqual(sorted(p.name for p in tmp.iterdir()), ["a.wav", "clips.json"])

    def test_committed_cases_are_well_formed(self):
        manifest = json.loads((HERE / "cases.json").read_text(encoding="utf-8"))
        ids = [c["id"] for c in manifest["cases"]]
        self.assertEqual(len(ids), len(set(ids)))
        for case in manifest["cases"]:
            if case["feature"] == "no_speech":
                self.assertEqual(case["language"], "any")
                self.assertEqual(case["expect"], {"equals": "", "send": False})
                self.assertGreaterEqual(case["noise"]["seconds"], 1)
                self.assertLessEqual(case["noise"]["seconds"], 3)
            elif case["feature"] == "long_form":
                reference = score.long_form_reference(case, manifest["scripts"])
                self.assertLessEqual(case["sentences"], len(manifest["scripts"][case["script"]]))
                # The bare-word check only means something if nobody says it.
                self.assertNotIn("language", reference.lower())
                self.assertEqual(case["expect"]["absent_words"], ["language"])
            elif case["feature"] == "code_switch" or "terms" in case.get("expect", {}):
                self.assertEqual(case["language"], "zh")
                for term in case["expect"]["terms"]:
                    self.assertTrue(score.has_term(case["say"], term), (case["id"], term))
                if case["expect"].get("cjk_punct"):
                    self.assertEqual(score.half_width_in_chinese(case["reference"]), [], case["id"])


if __name__ == "__main__":
    unittest.main()

import json
from pathlib import Path
import tempfile
import unittest

from engine_regression_audit import audit
from engine_regression_profile import propose
from test_engine_regression_profile import SOURCE, BEFORE, AFTER


class AuditTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        for directory in ("sql", "expected", "reference", "candidate", "profiles"):
            (self.root / directory).mkdir()
        (self.root / "parallel_schedule").write_text("test: one\n")
        (self.root / "sql/one.sql").write_text(SOURCE)
        (self.root / "expected/one.out").write_text(BEFORE)
        (self.root / "reference/one.out").write_text(BEFORE)
        (self.root / "candidate/one.out").write_text(AFTER)
        self.profile = propose(SOURCE, BEFORE, AFTER, "reviewed plan")
        (self.root / "profiles/one.json").write_text(json.dumps(self.profile))

    def run_audit(self):
        return audit(self.root, self.root / "reference", self.root / "candidate",
                     self.root / "profiles", self.root / "output")

    def test_profile_has_separate_verdict(self):
        report = self.run_audit()
        self.assertTrue(report["passed"])
        self.assertFalse(report["cases"][0]["raw_equal"])

    def test_bad_native_cannot_become_control(self):
        (self.root / "reference/one.out").write_text(AFTER)
        self.assertFalse(self.run_audit()["passed"])

    def test_missing_scheduled_case_blocks(self):
        (self.root / "parallel_schedule").write_text("test: one two\n")
        report = self.run_audit()
        self.assertEqual(report["total"], 2)
        self.assertEqual(report["unresolved"], ["two"])

    def test_unreviewed_plan_fails(self):
        (self.root / "profiles/one.json").unlink()
        self.assertFalse(self.run_audit()["passed"])

    def test_new_runtime_difference_fails_without_updating_profile(self):
        (self.root / "candidate/one.out").write_text(AFTER.replace("   Gravity", "   Other Engine"))
        self.assertFalse(self.run_audit()["passed"])
        self.assertEqual(json.loads((self.root / "profiles/one.json").read_text()), self.profile)

    def test_stale_profile_fails_even_when_native_matches(self):
        self.profile["source_sha256"] = "wrong"
        (self.root / "profiles/one.json").write_text(json.dumps(self.profile))
        self.assertFalse(self.run_audit()["passed"])

    def test_identical_unparsed_output_stays_valid(self):
        (self.root / "profiles/one.json").unlink()
        for path in ("expected/one.out", "reference/one.out", "candidate/one.out"):
            (self.root / path).write_text("feature disabled; skipped\n")
        self.assertTrue(self.run_audit()["passed"])

    def test_whole_pinned_alternative_allowed(self):
        (self.root / "profiles/one.json").unlink()
        (self.root / "expected/one_1.out").write_text(AFTER)
        report = self.run_audit()
        self.assertTrue(report["passed"])
        self.assertEqual(report["cases"][0]["candidate_expectation"], "one_1.out")


if __name__ == "__main__":
    unittest.main()

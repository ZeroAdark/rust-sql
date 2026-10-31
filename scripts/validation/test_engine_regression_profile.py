import copy
import unittest
from unittest.mock import patch

from compare_regression import compare
from engine_regression_profile import apply, digest, plan_tables, propose


SOURCE = "EXPLAIN SELECT 1;\nSELECT 1;\n"
PG = "QUERY PLAN\n----------\n Result\n(1 row)\n"
ENGINE = " QUERY PLAN \n------------\n Result\n   Gravity\n(2 rows)\n"
ROW = " ?column? \n----------\n        1\n(1 row)\n"
BEFORE = "EXPLAIN SELECT 1;\n" + PG + "\nSELECT 1;\n" + ROW
AFTER = BEFORE.replace(PG, ENGINE)


class EngineProfileTests(unittest.TestCase):
    def test_scoped_choice_does_not_bless_other_changes(self):
        sql = "SELECT a,b FROM t ORDER BY coalesce(a,0)+coalesce(b,0);"
        source = "EXPLAIN SELECT 1;\n" + sql + "\n"
        before = "EXPLAIN SELECT 1;\n" + PG + "\n" + sql + "\n a | b \n---+---\n 1 | 1\n 2 |  \n(2 rows)\n"
        after = before.replace(PG, ENGINE).replace(" 1 | 1\n 2 |  \n", " 2 |  \n 1 | 1\n")
        entry = {digest(source): {"expected_sha256": digest(before), "statements": {
            digest(sql): {"kind": "grouping_coalesce_sum", "columns": ["a", "b"]}}}}
        with patch("choice_witness.policy", return_value=entry):
            profile = propose(source, before, after, "plan and independently proven peer order")
            self.assertEqual(len(profile["plans"]), 1)
            self.assertIn(" 1 | 1\n 2 |  \n", apply(source, before, profile))
            for invalid in (after.replace(" 1 | 1", " 1 | 2"), after + "ERROR: failure\n"):
                with self.assertRaises(ValueError):
                    propose(source, before, invalid, "invalid")

    def setUp(self):
        self.profile = propose(SOURCE, BEFORE, AFTER, "Explicit engine plan")

    def test_exact_plan_and_results(self):
        self.assertEqual(apply(SOURCE, BEFORE, self.profile), AFTER)
        self.assertEqual(len(self.profile["plans"]), 1)

    def test_changed_value_not_recorded(self):
        with self.assertRaisesRegex(ValueError, "non-plan differences"):
            propose(SOURCE, BEFORE, AFTER.replace("        1", "        2"), "bad value")

    def test_changed_value_fails_existing_profile(self):
        report, _ = compare(SOURCE, apply(SOURCE, BEFORE, self.profile), AFTER.replace("        1", "        2"))
        self.assertFalse(report["equal_after_permitted_order"])

    def test_notice_and_error_not_swallowed(self):
        for message in ("NOTICE: changed\n", "ERROR: failed\n"):
            with self.subTest(message=message), self.assertRaises(ValueError):
                propose(SOURCE, BEFORE, AFTER.replace("SELECT 1;\n" + ROW, "SELECT 1;\n" + message + ROW), "bad")

    def test_source_and_expected_hashes_bound(self):
        for source, expected in ((SOURCE + "SELECT 2;\n", BEFORE), (SOURCE, BEFORE + "NOTICE: different\n")):
            with self.assertRaisesRegex(ValueError, "changed"):
                apply(source, expected, self.profile)

    def test_helper_and_fake_plan_header_not_eligible(self):
        source = "SELECT explain_helper();\n"
        before, after = source + PG, source + ENGINE
        self.assertEqual(plan_tables(source, before), {})
        with self.assertRaises(ValueError):
            propose(source, before, after, "helper must be reviewed separately")

    def test_no_adjacent_statement_in_replacement(self):
        profile = copy.deepcopy(self.profile)
        profile["plans"][0]["after"] += "SELECT 2;\n"
        with self.assertRaisesRegex(ValueError, "exactly one"):
            apply(SOURCE, BEFORE, profile)

    def test_no_unaligned_error_in_plan(self):
        profile = copy.deepcopy(self.profile)
        profile["plans"][0]["after"] = ENGINE.replace("   Gravity", "ERROR: failure")
        with self.assertRaises(ValueError):
            apply(SOURCE, BEFORE, profile)

    def test_duplicate_replacement_rejected(self):
        self.profile["plans"] *= 2
        with self.assertRaisesRegex(ValueError, "duplicate"):
            apply(SOURCE, BEFORE, self.profile)

    def test_missing_echo_fails_closed(self):
        with self.assertRaisesRegex(ValueError, "missing SQL echo"):
            propose(SOURCE, BEFORE, AFTER.replace("EXPLAIN SELECT 1;", "SELECT 0;"), "bad")

    def test_original_plan_hash_bound(self):
        self.profile["plans"][0]["before_sha256"] = digest("different")
        with self.assertRaisesRegex(ValueError, "table changed"):
            apply(SOURCE, BEFORE, self.profile)

    def test_duplicate_sql_occurrences_bound(self):
        source = "EXPLAIN SELECT 1;\n" * 2
        before = ("EXPLAIN SELECT 1;\n" + PG + "\n") * 2
        actual = before.replace(PG, ENGINE, 1)
        profile = propose(source, before, actual, "first occurrence only")
        self.assertEqual(profile["plans"][0]["occurrence"], 1)
        self.assertEqual(apply(source, before, profile), actual)

    def test_row_permutation_preserves_base_result(self):
        source = "EXPLAIN SELECT 1;\nSELECT x FROM t;\n"
        rows = " x \n---\n 1\n 2\n(2 rows)\n"
        before = "EXPLAIN SELECT 1;\n" + PG + "\nSELECT x FROM t;\n" + rows
        after = before.replace(PG, ENGINE).replace(" 1\n 2\n", " 2\n 1\n")
        profile = propose(source, before, after, "plan only")
        self.assertIn(rows, apply(source, before, profile))
        self.assertTrue(compare(source, apply(source, before, profile), after)[0]["equal_after_permitted_order"])


if __name__ == "__main__":
    unittest.main()

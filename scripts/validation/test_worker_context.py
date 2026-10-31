from pathlib import Path
import unittest
from unittest.mock import patch

from compare_regression import compare, find_anchor, physical_lines, statements
from diagnostic_policy import guard_change
from worker_context import compare_worker_context, digest, policy


ROOT = Path(__file__).resolve().parents[2] / "crates/postgres-18.6-reference/src/test/regress"


class WorkerContextTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.source = (ROOT / "sql/select_parallel.sql").read_text()
        cls.expected = (ROOT / "expected/select_parallel.out").read_text()
        cls.actual = cls.expected.replace("CONTEXT:  parallel worker\n", "").replace("\nparallel worker\n", "\n")

    def test_pinned_source_statements_and_native_output(self):
        entry = policy()
        self.assertEqual(digest(self.source), entry["source_sha256"])
        self.assertEqual(digest(self.expected), entry["expected_sha256"])
        found = {line: sql for line, _, sql in statements(self.source)}
        for rule in entry["statements"]:
            self.assertEqual(found[rule["sql_line"]], rule["sql"])
            self.assertEqual(sum(sql == rule["sql"] for line, sql in found.items() if line <= rule["sql_line"]), rule["occurrence"])

    def test_only_two_worker_entries_can_be_absent(self):
        normalized, proofs = compare_worker_context(self.source, self.expected, self.actual)
        self.assertEqual(normalized, self.expected)
        self.assertEqual([p["sql_line"] for p in proofs], [460,547])
        self.assertEqual(compare_worker_context(self.source, self.expected, self.expected), (self.expected, []))

    def test_primary_error_full_length_function_context_and_other_occurrence_stay_strict(self):
        alternatives = [self.actual.replace("division by zero", "numeric value out of range"),
                        self.actual.replace('CONTEXT:  SQL function "set_role_and_error" statement 1\n', ""),
                        self.actual.replace('SQL function "set_role_and_error"', 'SQL function "wrong_function"'),
                        self.actual.replace("BAAAAAabcd", "BAAAAAbcd", 1),
                        self.actual.replace("ERROR:  division by zero\n", "", 1),
                        self.actual + "ERROR:  unrelated failure\n"]
        for altered in alternatives:
            with self.subTest(altered=altered[-60:]):
                checked, _ = compare_worker_context(self.source, self.expected, altered)
                self.assertFalse(compare(self.source, self.expected, checked)[0]["equal_after_permitted_order"])

    def test_changed_source_or_native_never_authorizes_context_omission(self):
        for source, expected in ((self.source + "-- drift\n", self.expected), (self.source, self.expected + "\n")):
            self.assertEqual(compare_worker_context(source, expected, self.actual), (self.actual, []))

    def test_line_endings_notice_and_missing_echo_stay_strict(self):
        variants = [self.actual.replace("ERROR:  division by zero\n", "ERROR:  division by zero\r\n"),
                    self.actual.replace("ERROR:  division by zero\n", "NOTICE:  unexpected\nERROR:  division by zero\n"),
                    self.actual.replace(policy()["statements"][0]["sql"], "select 1;", 1)]
        for actual in variants:
            normalized, _ = compare_worker_context(self.source, self.expected, actual)
            self.assertNotEqual(normalized, self.expected)

    def test_unrecognized_omission_is_not_a_general_context_mask(self):
        data = {**policy(), "statements": [dict(rule) for rule in policy()["statements"]]}
        data["statements"][0]["omitted_context"] = 'CONTEXT:  SQL function "set_role_and_error" statement 1\n'
        with patch("worker_context.policy", return_value=data), self.assertRaises(ValueError):
            compare_worker_context(self.source, self.expected, self.actual)

    def test_worker_statistics_have_exact_boolean_shape(self):
        rule = {"kind":"postgres_worker_activity","columns":["wrk_to_launch","wrk_launched"]}
        guard_change([" t | t"],[" f | f"],rule)
        for after in ([],[" f | f"," f | f"],[" f | t"],[" t | f"],[" t | t"],["false|false"],[" f | \tf"],[" f | f | f"],["   |  "]):
            with self.subTest(after=after), self.assertRaises(ValueError):guard_change([" t | t"],after,rule)
        for before in ([],["t|f"],["t|t","t|t"]):
            with self.assertRaises(ValueError):guard_change(before,["f|f"],rule)
        with self.assertRaises(ValueError):guard_change(["t|t"],["f|f"],{**rule,"columns":["other","column"]})


if __name__ == "__main__":
    unittest.main()

from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from aggregate_strategy import DATA, ROOT, compare_aggregate_strategy, digest, fixture_sum, policy
from compare_regression import find_anchor, physical_lines, statements, table
from engine_regression_profile import propose


FIXTURES = ROOT / "crates/postgres-18.6-reference/src/test/regress"


def synthetic_serial(expected, source):
    lines, pos = physical_lines(expected, keepends=True), 0
    for _, anchor, sql in statements(source):
        start = find_anchor(physical_lines("".join(lines)), anchor, pos)
        pos = start + len(anchor)
        if sql == policy()["explain"]:
            end, _ = table(physical_lines("".join(lines)), pos)
            lines[pos:end + 1] = ["       QUERY PLAN       \n", "------------------------\n",
                                  " Aggregate\n", "   ->  Seq Scan on tenk1\n", "(2 rows)\n"]
            break
    with_native_value = "".join(lines)
    target = policy()["sql"] + "\n balk \n------\n     \n(1 row)\n"
    before, sep, after = with_native_value.rpartition(target)
    assert sep
    return (before + policy()["sql"] + "\n  balk  \n--------\n 495000\n(1 row)\n" + after,
            with_native_value)


class AggregateStrategyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.source = (FIXTURES / "sql/aggregates.sql").read_text()
        cls.expected = (FIXTURES / "expected/aggregates.out").read_text()
        cls.actual, cls.normalized = synthetic_serial(cls.expected, cls.source)

    def test_pinned_source_input_and_occurrence(self):
        rule = policy()
        self.assertEqual(digest(self.source), rule["source_sha256"])
        self.assertEqual(digest(self.expected), rule["expected_sha256"])
        calls = [(line, sql) for line, _, sql in statements(self.source) if sql == rule["sql"]]
        self.assertEqual(len(calls), 2)
        self.assertEqual(calls[1][0], rule["sql_line"])
        self.assertEqual(fixture_sum(DATA, rule["data_lf_sha256"]), 495000)

    def test_serial_result_and_original_native_are_valid(self):
        normalized, proofs = compare_aggregate_strategy(self.source, self.expected, self.actual)
        self.assertEqual(normalized, self.normalized)
        self.assertEqual(len(proofs), 1)
        self.assertEqual(proofs[0]["occurrence"], 2)
        self.assertEqual(proofs[0]["serial_sum"], 495000)
        self.assertEqual(compare_aggregate_strategy(self.source, self.expected, self.expected), (self.expected, []))
        profile = propose(self.source, self.expected, self.actual, "test serial strategy")
        self.assertEqual(len(profile["plans"]), 1)

    def test_wrong_sum_null_duplicate_or_malformed_result_fails(self):
        for text in (" 494999", "      0", "       ", " 495000\n 495000", " 495000\t", " 0495000"):
            with self.subTest(text=text), self.assertRaises(ValueError):
                propose(self.source, self.expected, self.actual.replace(" 495000\n", text + "\n"), "invalid")
        # A serial NULL must fail even though it equals the parallel native value.
        with self.assertRaises(ValueError):
            propose(self.source, self.expected, self.normalized, "invalid serial NULL")

    def test_partial_plan_cannot_authorize_serial_value(self):
        for replacement in (" Finalize Aggregate", " Partial Aggregate", " Aggregate\n   -> Gather"):
            start = self.actual.index(policy()["explain"]) + len(policy()["explain"])
            altered = self.actual[:start] + self.actual[start:].replace(
                " Aggregate\n   ->  Seq Scan on tenk1", replacement + "\n   ->  Seq Scan on tenk1", 1)
            with self.subTest(replacement=replacement), self.assertRaises(ValueError):
                propose(self.source, self.expected, altered, "invalid")

    def test_first_null_transition_and_error_tail_remain_exact(self):
        target = policy()["sql"] + "\n balk \n------\n     \n(1 row)\n"
        first_changed = self.actual.replace(target, target.replace("     \n", "    1\n"), 1)
        for altered in (first_changed, self.actual + "ERROR: an unrelated failure\n",
                        self.actual.replace(" 495000\n", " 495000\r\n")):
            with self.assertRaises(ValueError):
                propose(self.source, self.expected, altered, "invalid")

    def test_source_native_input_or_intervening_notice_cannot_be_masked(self):
        for source, expected in ((self.source + "-- drift\n", self.expected), (self.source, self.expected + "NOTICE: drift\n")):
            self.assertEqual(compare_aggregate_strategy(source, expected, self.actual), (self.actual, []))
        altered = self.actual.replace("(2 rows)\n\n" + policy()["sql"], "(2 rows)\nNOTICE: intervene\n\n" + policy()["sql"], 1)
        self.assertEqual(compare_aggregate_strategy(self.source, self.expected, altered), (altered, []))
        with patch("aggregate_strategy.DATA", Path("/missing-fixture")), self.assertRaises(OSError):
            compare_aggregate_strategy(self.source, self.expected, self.actual)
        with patch("aggregate_strategy.SETUP", DATA), self.assertRaises(ValueError):
            compare_aggregate_strategy(self.source, self.expected, self.actual)

    def test_input_schema_values_and_hash_are_checked(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "tenk.data"
            for contents in ("1\t2\n", "\t".join(["0"] * 6 + ["100"] + ["0"] * 9) + "\n",
                             DATA.read_text().replace("\t", " ", 1), DATA.read_text() + "\n"):
                path.write_text(contents)
                with self.assertRaises(ValueError):
                    fixture_sum(path, policy()["data_lf_sha256"])
            with self.assertRaises(ValueError):
                fixture_sum(DATA, "0" * 64)


if __name__ == "__main__":
    unittest.main()

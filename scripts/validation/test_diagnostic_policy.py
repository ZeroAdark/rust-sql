import copy
from decimal import Decimal
import json
from pathlib import Path
import unittest

from compare_regression import physical_lines
from diagnostic_policy import bindings, display_table, guard_change, json_object, policy, root_count
from engine_regression_profile import plan_tables, validate_plan


TEXT = {"kind": "text_plan", "header": "helper"}
JSON = {"kind": "incremental_sort_json", "header": "jsonb_pretty"}
MEMORY = {"kind": "memory_context_absence", "columns": ["type", "name", "?column?", "total_nblocks", "?column?", "free_chunks"]}


def json_table(nodes):
    body = json.dumps(nodes, indent=4).splitlines()
    width = max(len(line) for line in body) + 3
    return ("jsonb_pretty".center(width) + "\n" + "-" * width + "\n" +
            "\n".join(" " + line.ljust(width - 2) + "+" for line in body[:-1]) +
            "\n " + body[-1] + "\n(1 row)\n")


class DiagnosticPolicyTests(unittest.TestCase):
    def test_only_pinned_sources_receive_bindings(self):
        self.assertEqual(bindings("SELECT explain_helper();\n"), {})

    def test_reviewed_definitions_and_call_sites_exist(self):
        root = Path(__file__).resolve().parents[2] / "crates/postgres-18.6-reference/src/test/regress"
        for entry in policy().values():
            source = (root / "sql" / (entry["case"] + ".sql")).read_text()
            self.assertEqual(bindings(source), entry["statements"])
            self.assertEqual(bindings(source + "-- changed\n"), {})
            tables = plan_tables(source, (root / "expected" / (entry["case"] + ".out")).read_text())
            self.assertTrue(set(entry["statements"]) <= {key[0] for key in tables})

    def test_root_count_cannot_change(self):
        with self.assertRaisesRegex(ValueError, "root rows"):
            guard_change([" Limit (actual rows=3.00 loops=1)"], [" Limit (actual rows=4.00 loops=1)"], TEXT)

    def test_root_count_cannot_disappear(self):
        with self.assertRaises(ValueError):
            guard_change([" Limit (actual rows=3.00 loops=1)"], [" Limit"], TEXT)

    def test_root_loops_cannot_change(self):
        with self.assertRaises(ValueError):
            guard_change([" Aggregate (actual rows=1.00 loops=1)"], [" Aggregate (actual rows=1.00 loops=2)"], TEXT)

    def test_masked_upstream_loop_marker_preserved(self):
        guard_change([" Aggregate (actual rows=1.00 loops=N)"], [" Other (actual rows=1.00 loops=N)"], TEXT)

    def test_unknown_root_instrumentation_refused(self):
        for line in (" Result (actual: not counted)", " Result (actual rows=-1 loops=1)"):
            with self.assertRaises(ValueError):
                root_count([line])

    def test_valid_json_with_exact_counts(self):
        value = [{"Node Type": "Incremental Sort", "Actual Rows": 55, "Actual Loops": 1}]
        parsed = validate_plan(json_table(value), JSON)
        self.assertEqual(parsed, value)
        guard_change(value, parsed, JSON)

    def test_json_rows_and_loops_are_protected(self):
        before = [{"Node Type": "Incremental Sort", "Actual Rows": 55, "Actual Loops": 1}]
        for field in ("Actual Rows", "Actual Loops"):
            after = copy.deepcopy(before)
            after[0][field] += 1
            with self.assertRaises(ValueError):
                guard_change(before, after, JSON)

    def test_non_plan_json_rejected(self):
        for nodes in ([], [1], [{"Node Type": "ordinary data"}], {"Node Type": "Incremental Sort"}):
            with self.subTest(nodes=nodes), self.assertRaises(ValueError):
                validate_plan(json_table(nodes), JSON)

    def test_bad_json_count_types_rejected(self):
        for count in (True, "55", -1, None, float("nan"), float("inf")):
            value = [{"Node Type": "Incremental Sort", "Actual Rows": count, "Actual Loops": 1}]
            with self.subTest(count=count), self.assertRaises(ValueError):
                validate_plan(json_table(value), JSON)

    def test_missing_continuation_marker_rejected(self):
        value = [{"Node Type": "Incremental Sort", "Actual Rows": 55, "Actual Loops": 1}]
        with self.assertRaises(ValueError):
            validate_plan(json_table(value).replace("+\n", "\n", 1), JSON)

    def test_extra_statement_or_wrong_footer_rejected(self):
        value = [{"Node Type": "Incremental Sort", "Actual Rows": 55, "Actual Loops": 1}]
        for text in (json_table(value) + "SELECT 1;\n", json_table(value).replace("(1 row)", "(2 rows)")):
            with self.assertRaises(ValueError):
                validate_plan(text, JSON)

    def test_allocator_observation_only_accepts_absence(self):
        guard_change([" Bump | Caller tuples"], [], MEMORY)
        with self.assertRaises(ValueError):
            guard_change([], [" fabricated context"], MEMORY)

    def test_wrong_header_rejected(self):
        self.assertIsNone(display_table(physical_lines("ordinary_data\n-------------\n Result\n(1 row)\n"), 0, TEXT))

    def test_decimal_root_count_is_exact(self):
        self.assertEqual(root_count([" Limit (actual rows=55.00 loops=1)"]), (Decimal("55.00"), "1"))

    def test_duplicate_jsonb_keys_rejected(self):
        with self.assertRaises(ValueError):
            json_object([("Actual Rows", 99), ("Actual Rows", 55)])


if __name__ == "__main__":
    unittest.main()

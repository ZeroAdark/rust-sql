from pathlib import Path
from itertools import permutations, product
import unittest
from unittest.mock import patch

from choice_witness import REPRESENTATIVES, compare_choices, digest, policy, prove
from compare_regression import columns, find_anchor, physical_lines, statements, table


class ChoiceWitnessTests(unittest.TestCase):
    def test_integer_sum_peers_keep_all_values_and_key_order(self):
        rows = [" | |0", "1| |1", "2| |2", "1|1|3", "3| |4", "1|2|5"]
        spans = [(0, 1), (2, 3), (4, 5)]
        for middle, final in product(permutations(rows[2:4]), permutations(rows[4:6])):
            self.assertTrue(prove("grouping_coalesce_sum", rows, rows[:2] + list(middle) + list(final), spans))
        for actual in ([rows[1], rows[0], *rows[2:]], [*rows[:2], rows[2], rows[2], *rows[4:]],
                       [*rows[:-1], "1|2|6"], [rows[0], rows[4], *rows[2:4], rows[1], rows[5]]):
            self.assertFalse(prove("grouping_coalesce_sum", rows, actual, spans))

    def test_integer_sum_refuses_rounded_or_overflowing_keys(self):
        for values in (("1.0", "1"), ("01", "1"), ("2147483647", "1"), ("2147483648", "0")):
            row = f"{values[0]:<12}|{values[1]:<12}"
            self.assertFalse(prove("grouping_coalesce_sum", [row], [row], [(0, 12), (13, 25)]))

    def test_infinite_peers_preserve_every_row(self):
        rows = ["(0,0)", "(1e+300,Infinity)", "(Infinity,1e+300)", "(NaN,NaN)"]
        spans = [(0, 30)]
        self.assertTrue(prove("infinite_point_distance", rows, [rows[0], rows[2], rows[1], rows[3]], spans))
        for actual in ([rows[0], rows[3], rows[1], rows[2]], [rows[0], rows[2], rows[2], rows[3]],
                       ["(0,1)", *rows[1:]]):
            self.assertFalse(prove("infinite_point_distance", rows, actual, spans))

    def test_finite_rounding_does_not_create_peers(self):
        rows = ["(1,0)", "(-1,0)"]
        self.assertFalse(prove("infinite_point_distance", rows, list(reversed(rows)), [(0, 10)]))

    def test_geometry_respects_all_sort_keys(self):
        def row(circle, point, distance):
            return f"{circle:<20}|{point:<22}|{distance:<12}"
        rows = [row("<(1,2),3>", "(1e+300,Infinity)", "Infinity"),
                row("<(5,1),3>", "(1e+300,Infinity)", "Infinity"),
                row("<(1,2),3>", "(Infinity,1e+300)", "Infinity")]
        spans = [(0, 20), (21, 43), (44, 56)]
        self.assertTrue(prove("circle_special_distance", rows, [rows[1], rows[0], rows[2]], spans))
        self.assertFalse(prove("circle_special_distance", rows, [rows[2], rows[1], rows[0]], spans))
        altered = [rows[0].replace("<(1,2),3>", "<(1,2),5>"), *rows[1:]]
        self.assertFalse(prove("circle_special_distance", rows, altered, spans))

    def test_representative_membership_and_unique_group(self):
        expected = ["3|2|1", "5|1|2", "3|1|3"]
        actual = ["3|2|1", "3|2|2", "3|2|3"]
        spans = [(0, 1), (2, 3), (4, 5)]
        self.assertTrue(prove("srf_distinct_representative", expected, actual, spans))
        for invalid in (["9|9|1", *actual[1:]], [actual[0], actual[0], actual[2]],
                        [*actual[:2], "3|2|4"], [*actual[:2], "3|2|3garbage"]):
            self.assertFalse(prove("srf_distinct_representative", expected, invalid, spans))

    def test_json_exact_elements_duplicates_and_partition_consistency(self):
        bars = [None, 3, 1, None, None, 5, 2, 4, None, 5, 4]
        def rows(odd):
            return [(" |" if bar is None else f"{bar}|" + (odd if bar % 2 else "[4, 4]")) for bar in bars]
        expected, actual = rows("[5, 3, 5]"), rows("[3, 5, 5]")
        spans = [(0, 1), (2, 20)]
        self.assertTrue(prove("json_full_partition", expected, actual, spans))
        for text in ("[3, 5]", "[3, 5, 6]", "[3, 5.0, 5]", '[3, "5", 5]', "[3, true, 5]", "[3,5,5]"):
            self.assertFalse(prove("json_full_partition", expected, rows(text), spans), text)
        actual[1] = "3|[5, 3, 5]"
        self.assertFalse(prove("json_full_partition", expected, actual, spans))

    def test_null_partition_and_input_multiplicity(self):
        rows = [" |", "3|[3, 5, 5]", "1|[3, 5, 5]", " |", " |", "5|[3, 5, 5]",
                "2|[4, 4]", "4|[4, 4]", " |", "5|[3, 5, 5]", "4|[4, 4]"]
        for actual in ([" |[]", *rows[1:]], [rows[1], *rows[1:]], rows[:-1]):
            self.assertFalse(prove("json_full_partition", rows, actual, [(0, 1), (2, 20)]))

    def test_all_representative_choices_and_duplicate_group_mutations(self):
        expected = ["3|2|1", "5|1|2", "3|1|3"]
        spans = [(0, 1), (2, 3), (4, 5)]
        for choices in product(sorted(REPRESENTATIVES), repeat=3):
            actual = [f"{a}|{b}|{g}" for (a, b), g in zip(choices, (1, 2, 3))]
            self.assertTrue(prove("srf_distinct_representative", expected, actual, spans))
            self.assertFalse(prove("srf_distinct_representative", expected, [actual[0], actual[0], actual[2]], spans))

    def test_every_odd_array_permutation_is_valid_only_when_consistent(self):
        import json
        bars = [None, 3, 1, None, None, 5, 2, 4, None, 5, 4]
        def rows(array):
            return [" |" if b is None else f"{b}|" + (json.dumps(array) if b % 2 else "[4, 4]") for b in bars]
        expected = rows([3, 5, 5])
        for array in set(permutations([3, 5, 5])):
            self.assertTrue(prove("json_full_partition", expected, rows(array), [(0, 1), (2, 20)]))

    def test_unregistered_source_is_unchanged(self):
        self.assertEqual(compare_choices("SELECT 1;", "before", "after"), ("after", []))

    def test_scoped_changes_do_not_hide_errors_or_line_endings(self):
        source = "SELECT DISTINCT ON (g) a,b,g FROM known_values;\n"
        head = " a | b | g \n---+---+---\n"
        before = source + head + " 3 | 2 | 1\n 5 | 1 | 2\n 3 | 1 | 3\n(3 rows)\n"
        after = before.replace(" 5 | 1 | 2", " 3 | 2 | 2").replace(" 3 | 1 | 3", " 3 | 2 | 3")
        entry = {digest(source): {"expected_sha256": digest(before), "statements": {
            digest(source.rstrip("\n")): {"kind": "srf_distinct_representative", "columns": ["a", "b", "g"]}}}}
        with patch("choice_witness.policy", return_value=entry):
            self.assertEqual(compare_choices(source, before, after)[0], before)
            normalized, proofs = compare_choices(source, before, after + "ERROR: distinct failure\n")
            self.assertEqual(normalized, before + "ERROR: distinct failure\n")
            self.assertEqual(len(proofs), 1)
            mixed_eol = after.replace(" 3 | 2 | 2\n", " 3 | 2 | 2\r\n")
            self.assertEqual(compare_choices(source, before, mixed_eol), (mixed_eol, []))
            self.assertEqual(compare_choices(source, before + "NOTICE: changed\n", after), (after, []))

    def test_policy_binds_real_queries_and_native_values(self):
        root = Path(__file__).resolve().parents[2] / "crates/postgres-18.6-reference/src/test/regress"
        for source_hash, entry in policy().items():
            source = (root / "sql" / (entry["case"] + ".sql")).read_text()
            expected = (root / "expected" / (entry["case"] + ".out")).read_text()
            self.assertEqual(digest(source), source_hash)
            self.assertEqual(digest(expected), entry["expected_sha256"])
            lines, position, found = physical_lines(expected), 0, set()
            for _, anchor, sql in statements(source):
                start = find_anchor(lines, anchor, position)
                self.assertIsNotNone(start)
                position = start + len(anchor)
                rule = entry["statements"].get(digest(sql))
                if rule:
                    end, rows = table(lines, position)
                    labels, spans = columns(lines[position], lines[position + 1])
                    self.assertEqual(labels, rule["columns"])
                    self.assertTrue(prove(rule["kind"], rows, rows, spans))
                    found.add(digest(sql))
            self.assertEqual(found, set(entry["statements"]))


if __name__ == "__main__":
    unittest.main()

import random
import unittest
from pathlib import Path

from compare_regression import ORDER, compare, find_anchor, statements


def output(sql, rows, header=" k | v ", delimiter="---+---"):
    return sql + "\n" + header + "\n" + delimiter + "\n" + "\n".join(rows) + f"\n({len(rows)} rows)\n"


class ComparisonTests(unittest.TestCase):
    def verdict(self, sql, expected, actual):
        return compare(sql + "\n", output(sql, expected), output(sql, actual))[0]["equal_after_permitted_order"]

    def test_unordered_permutation_preserves_duplicates(self):
        self.assertTrue(self.verdict("SELECT k,v FROM t;", [" 1 | a", " 1 | a", " 2 | b"], [" 2 | b", " 1 | a", " 1 | a"]))

    def test_value_change_and_duplicate_loss_rejected(self):
        for rows in ([" 2 | b", " 1 | a", " 2 | b"], [" 2 | c", " 1 | a", " 1 | a"]):
            self.assertFalse(self.verdict("SELECT k,v FROM t;", [" 1 | a", " 1 | a", " 2 | b"], rows))

    def test_unicode_separators_are_data_not_physical_lines(self):
        for separator in ("\u2028", "\u2029", "\v", "\f"):
            first = " 1 | a" + separator + "b"
            self.assertTrue(self.verdict("SELECT k,v FROM t;", [first, " 2 | c"], [" 2 | c", first]))
            parsed = list(statements("SELECT 'a" + separator + "b';\nSELECT 2;\n"))
            self.assertEqual([p[0] for p in parsed], [1, 2])

    def test_row_line_ending_changes_stay_exact(self):
        sql = "SELECT k,v FROM t;"
        expected = output(sql, [" 1 | a", " 2 | b"])
        actual = output(sql, [" 2 | b\r", " 1 | a\r"])
        self.assertFalse(compare(sql, expected, actual)[0]["equal_after_permitted_order"])

    def test_peer_permutation_only(self):
        sql = "SELECT k,v FROM t ORDER BY k;"
        rows = [" 1 | a", " 1 | b", " 2 | c"]
        self.assertTrue(self.verdict(sql, rows, [" 1 | b", " 1 | a", " 2 | c"]))
        self.assertFalse(self.verdict(sql, rows, [" 2 | c", " 1 | b", " 1 | a"]))
        self.assertFalse(self.verdict(sql, rows, [" 1 | c", " 1 | a", " 2 | b"]))

    def test_all_keys_respected(self):
        self.assertFalse(self.verdict("SELECT k,v FROM t ORDER BY k,v;", [" 1 | a", " 1 | b"], [" 1 | b", " 1 | a"]))

    def test_limit_stays_exact(self):
        self.assertFalse(self.verdict("SELECT k,v FROM t LIMIT 2;", [" 1 | a", " 2 | b"], [" 2 | b", " 1 | a"]))

    def test_explain_stays_exact(self):
        self.assertFalse(self.verdict("EXPLAIN SELECT k,v FROM t;", [" 1 | a", " 2 | b"], [" 2 | b", " 1 | a"]))

    def test_nested_array_values_stay_exact(self):
        self.assertFalse(self.verdict("SELECT k,array_agg(v ORDER BY v) FROM t GROUP BY k;", [" 1 | {a,b}"], [" 1 | {b,a}"]))

    def test_error_notice_and_trailing_output_stay_exact(self):
        sql = "SELECT k,v FROM t;"
        expected = output(sql, [" 1 | a", " 2 | b"]) + "ERROR: first\n"
        actual = output(sql, [" 2 | b", " 1 | a"]) + "ERROR: second\n"
        self.assertFalse(compare(sql, expected, actual)[0]["equal_after_permitted_order"])
        self.assertFalse(compare(sql, expected, actual.replace("ERROR:", "NOTICE:"))[0]["equal_after_permitted_order"])

    def test_multiple_statements_on_a_line_stay_exact(self):
        self.assertFalse(self.verdict("SELECT 1; SELECT k,v FROM t;", [" 1 | a", " 2 | b"], [" 2 | b", " 1 | a"]))

    def test_meta_send_stays_exact(self):
        self.assertFalse(self.verdict("SELECT k,v FROM t \\g", [" 1 | a", " 2 | b"], [" 2 | b", " 1 | a"]))

    def test_multiline_values_stay_exact(self):
        sql = "SELECT k,v FROM t;"
        expected = output(sql, [" 1 | a+", "   | b", " 2 | c"]).replace("(3 rows)", "(2 rows)")
        actual = output(sql, [" 2 | c", " 1 | a+", "   | b"]).replace("(3 rows)", "(2 rows)")
        self.assertFalse(compare(sql, expected, actual)[0]["equal_after_permitted_order"])

    def test_duplicate_column_names_stay_exact(self):
        sql = "SELECT t.k,u.k FROM t,u ORDER BY t.k;"
        expected = output(sql, [" 1 | a", " 1 | b"], header=" k | k ")
        actual = output(sql, [" 1 | b", " 1 | a"], header=" k | k ")
        self.assertFalse(compare(sql, expected, actual)[0]["equal_after_permitted_order"])

    def test_repeated_statements_are_matched_in_sequence(self):
        sql = "SELECT k,v FROM t;"
        expected = output(sql, [" 1 | a", " 2 | b"])
        actual = output(sql, [" 2 | b", " 1 | a"])
        self.assertTrue(compare(sql + "\n" + sql, expected + expected, actual + actual)[0]["equal_after_permitted_order"])
        self.assertFalse(compare(sql + "\n" + sql, expected + expected, actual + actual.replace("2 | b", "2 | c"))[0]["equal_after_permitted_order"])

    def test_sql_annotation_does_not_mask_result(self):
        sql = "-- pgrust: ruled anything\nSELECT k,v FROM t;"
        self.assertFalse(self.verdict(sql, [" 1 | a"], [" 1 | b"]))

    def test_missing_echo_stops_acceptance(self):
        report, _ = compare("SELECT missing;\nSELECT k,v FROM t;", output("SELECT k,v FROM t;", [" 1 | a", " 2 | b"]), output("SELECT k,v FROM t;", [" 2 | b", " 1 | a"]))
        self.assertFalse(report["equal_after_permitted_order"])
        self.assertTrue(report["refused"])

    def test_numeric_text_is_not_rounded(self):
        self.assertFalse(self.verdict("SELECT k,sum(v::float8) FROM t GROUP BY k;", [" 1 | 1.0000000000001"], [" 1 | 1.0000000000002"]))

    def test_mutations_rejected_across_many_permutations(self):
        rng = random.Random(2910)
        original = [f" {i % 3} | {i}" for i in range(30)]
        for _ in range(60):
            changed = original[:]
            rng.shuffle(changed)
            self.assertTrue(self.verdict("SELECT k,v FROM t;", original, changed))
            changed[0] += "changed"
            self.assertFalse(self.verdict("SELECT k,v FROM t;", original, changed))


class ScannerTests(unittest.TestCase):
    def test_existing_order_classification_fixtures(self):
        fixtures = Path(__file__).resolve().parents[1] / "gravity-gate/fixtures/multiset"
        count = 0
        for path in fixtures.glob("*.case"):
            meta = {}
            for line in path.read_text().splitlines():
                if line.startswith("["):
                    break
                if ":" in line and not line.startswith("#"):
                    key, value = line.split(":", 1)
                    value = value[1:] if value.startswith(" ") else value
                    meta[key] = meta[key] + "\n" + value if key == "sql" and key in meta else value
            if "class" not in meta:
                continue
            expected = meta["class"].split()
            kind, keys = ORDER.classify(meta["sql"], meta["cols"].split("\t") if "cols" in meta else None)
            with self.subTest(path=path.name):
                self.assertEqual(kind, expected[0])
                self.assertEqual(keys or [], [int(k) for k in " ".join(expected[1:]).replace(",", " ").split()])
            count += 1
        self.assertGreater(count, 30)

    def test_literals_comments_and_dollar_bodies(self):
        source = "-- heading\nCREATE FUNCTION f() RETURNS text AS $tag$\nSELECT ';not an end';\n$tag$ LANGUAGE SQL;\n/* nested /* ; */ ; */\nSELECT 'a'';b', E'c\\\';d', \"semi;colon\"; -- end\nSELECT 1; SELECT 2;\nSELECT 3;\n"
        parsed = list(statements(source))
        self.assertEqual([p[0] for p in parsed], [2, 6, 8])
        self.assertEqual(len(parsed), 3)

    def test_unterminated_literals_do_not_emit(self):
        for sql in ("SELECT 'unclosed;", "SELECT $x$unclosed;", 'SELECT "unclosed;', "/* unclosed; SELECT 1;"):
            self.assertEqual(list(statements(sql)), [])

    def test_inline_meta_command_does_not_swallow_next_statement(self):
        parsed = list(statements("SELECT 1 \\gset\nSELECT 2;\n"))
        self.assertEqual([p[2] for p in parsed], ["SELECT 2;"])

    def test_empty_lines_follow_psql_echo_rules(self):
        parsed = list(statements("SELECT\n\n1;\nSELECT $x$a\n\nb$x$;\n"))
        self.assertEqual(parsed[0][1], ["SELECT", "1;"])
        self.assertEqual(parsed[1][1], ["SELECT $x$a", "", "b$x$;"])

    def test_copy_data_is_not_sql(self):
        parsed = list(statements("COPY t FROM STDIN;\nSELECT fake;\n\\.\nSELECT real;\n"))
        self.assertEqual([p[2] for p in parsed], ["COPY t FROM STDIN;", "SELECT real;"])

    def test_copy_filename_is_not_stdin(self):
        parsed = list(statements("COPY t FROM 'stdin';\nSELECT real;\n"))
        self.assertEqual(len(parsed), 2)

    def test_kmp_matches_naive_search(self):
        rng = random.Random(2901)
        for _ in range(300):
            lines = [rng.choice("abcd") for _ in range(80)]
            anchor = [rng.choice("abcd") for _ in range(rng.randrange(1, 12))]
            start = rng.randrange(len(lines))
            want = next((i for i in range(start, len(lines) - len(anchor) + 1) if lines[i:i + len(anchor)] == anchor), None)
            self.assertEqual(find_anchor(lines, anchor, start), want)


if __name__ == "__main__":
    unittest.main()

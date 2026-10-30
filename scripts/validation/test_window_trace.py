import copy
from itertools import permutations, product
import unittest

from compare_regression import columns, compare, find_anchor, physical_lines, statements, table
from window_trace import FIXTURES, compare_window_trace, digest, exclude, frame_values, inputs, policy, prove


class WindowTraceTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.source = (FIXTURES / 'sql/window.sql').read_text()
        cls.expected = (FIXTURES / 'expected/window.out').read_text()
        cls.raw = physical_lines(cls.expected, keepends=True)
        plain = physical_lines(cls.expected)
        cls.rules = {rule['sql_line']: rule for rule in policy()['statements']}
        cls.blocks = {}
        pos = 0
        for line, anchor, sql in statements(cls.source):
            start = find_anchor(plain, anchor, pos)
            pos = start + len(anchor)
            if line in cls.rules:
                end, rows = table(plain, pos)
                labels, spans = columns(plain[pos], plain[pos + 1])
                cells = [[row[a:b].strip(' ') for a, b in spans] for row in rows]
                cls.blocks[line] = (pos, end, labels, spans, cells)
        cls.data = inputs()

    def test_source_inputs_and_all_native_tables(self):
        self.assertEqual(len(self.rules), 24)
        self.assertEqual(digest(self.source), policy()['source_sha256'])
        self.assertEqual(digest(self.expected), policy()['expected_sha256'])
        commands = {line: sql for line, _, sql in statements(self.source)}
        for line, rule in self.rules.items():
            self.assertEqual(rule['sql'], commands[line])
            self.assertEqual(rule['columns'], self.blocks[line][2])
            self.assertIsNotNone(prove(rule, self.blocks[line][4], self.data), line)
            if rule['kind'] == 'frame':
                self.assertEqual(sorted(rule['input_columns'] + [f['column'] for f in rule['functions']]),
                                 list(range(len(rule['columns']))))

    def cells_for_order(self, rule, records):
        result = []
        for record, values in zip(records, frame_values(records, rule)):
            cells = [''] * len(rule['columns'])
            for column, value in zip(rule['input_columns'], record[0]):
                cells[column] = value
            for function, value in zip(rule['functions'], values):
                cells[function['column']] = '' if value is None else str(value)
            result.append(cells)
        return result

    def test_all_144_tenk_peer_orders_and_correlated_values(self):
        data = self.data['tenk_unique1']
        groups = [[row for row in data if row[1] == key] for key in range(4)]
        checked = 0
        for parts in product(*(list(permutations(group)) for group in groups)):
            records = [row for part in parts for row in part]
            for line in (214, 218, 226, 230, 234, 266):
                rule = self.rules[line]
                self.assertIsNotNone(prove(rule, self.cells_for_order(rule, records), self.data))
            checked += 1
        self.assertEqual(checked, 144)
        cells = copy.deepcopy(self.blocks[266][4])
        # Both are legal peer values in isolation, but first and second cannot
        # be the same unique row, and peers cannot disagree about the first.
        cells[0][1] = cells[0][0]
        self.assertIsNone(prove(self.rules[266], cells, self.data))

    def test_frame_bounds_and_exclusion_against_row_enumeration(self):
        records = [((str(i),), key, i) for i, key in enumerate((0, 0, 0, 2, 2, 5))]
        keys = [row[1] for row in records]
        group_keys = sorted(set(keys))
        for mode, lower, upper, exclusion in product(('ROWS', 'RANGE', 'GROUPS'),
                                                     (-2, 0, 1), (1, 3),
                                                     ('NO OTHERS', 'CURRENT ROW', 'GROUP', 'TIES')):
            rule = {'functions': [{'name': name, 'frame': [mode, lower, upper, exclusion]}
                                  for name in ('first', 'last', 'nth2')]}
            expected = []
            for i, key in enumerate(keys):
                selected = []
                for j, other in enumerate(keys):
                    position = j - i if mode == 'ROWS' else other - key if mode == 'RANGE' else group_keys.index(other) - group_keys.index(key)
                    removed = (exclusion == 'CURRENT ROW' and j == i or exclusion == 'GROUP' and other == key or
                               exclusion == 'TIES' and other == key and j != i)
                    if lower <= position <= upper and not removed:
                        selected.append(j)
                expected.append([selected[0] if selected else None, selected[-1] if selected else None,
                                 selected[1] if len(selected) > 1 else None])
            self.assertEqual(frame_values(records, rule), expected, (mode, lower, upper, exclusion))
        self.assertEqual(exclude(1, 3, 0, (0, 2), 'TIES'), [(2, 3)])
        self.assertEqual(exclude(0, 2, 2, (1, 3), 'TIES'), [(0, 1)])

    def test_missing_duplicate_changed_values_and_nulls_fail(self):
        for line, rule in self.rules.items():
            original = self.blocks[line][4]
            for altered in (original[:-1], original + [original[0]]):
                self.assertIsNone(prove(rule, altered, self.data), line)
            for i, row in enumerate(original):
                for j, value in enumerate(row):
                    altered = copy.deepcopy(original)
                    altered[i][j] = '999999' if value.lstrip('-').isdigit() or value == '' else 'unknown'
                    self.assertIsNone(prove(rule, altered, self.data), (line, i, j))
            if rule['kind'] == 'frame':
                swapped = copy.deepcopy(original)
                swapped[0], swapped[-1] = swapped[-1], swapped[0]
                self.assertIsNone(prove(rule, swapped, self.data), line)

    def test_all_partition_representatives_and_filtered_rank_completions(self):
        employees = self.data['employee_rows']
        personnel = [row for row in employees if row[0] == 'personnel']
        sales = [row for row in employees if row[0] == 'sales']
        for p, s in product(personnel, sales):
            cells = [[*row, str(n), '1', str(n), str(n), '1'] for row, n in ((p, 2), (s, 3))]
            self.assertIsNotNone(prove(self.rules[1415], cells, self.data))
        original = self.blocks[1652][4]
        index = next(i for i, row in enumerate(original) if row[1] == '7')
        for identity, first in product(('7', '9'), ('4', '5')):
            cells = copy.deepcopy(original)
            cells[index] = [*next(row for row in employees if row[1] == identity), first, '1']
            witness = prove(self.rules[1652], cells, self.data)
            self.assertIsNotNone(witness)
            self.assertEqual(witness['ascending']['develop'][int(first) - 1], int(identity))
            self.assertEqual(witness['descending']['develop'][0], int(identity))
        for altered in (original[:-1], original + [original[index]],
                        original + [[*next(row for row in employees if row[1] == '9'), '4', '1']]):
            self.assertIsNone(prove(self.rules[1652], altered, self.data))

    def test_scoped_replacement_preserves_errors_headers_and_source(self):
        line = 266
        start, end, _, spans, _ = self.blocks[line]
        rule = self.rules[line]
        records = sorted(self.data['tenk_unique1'], key=lambda row: (row[1], -row[2]))
        cells = self.cells_for_order(rule, records)
        rows = ['|'.join(' ' + value.rjust(b - a - 2) + ' ' for value, (a, b) in zip(row, spans)).rstrip() + '\n' for row in cells]
        actual = ''.join(self.raw[:start + 2] + rows + self.raw[end:])
        self.assertNotEqual(actual, self.expected)
        normalized, proofs = compare_window_trace(self.source, self.expected, actual)
        self.assertEqual(normalized, self.expected)
        self.assertEqual([p['sql_line'] for p in proofs], [line])
        for source, expected in ((self.source + '-- drift\n', self.expected), (self.source, self.expected + '\n')):
            self.assertEqual(compare_window_trace(source, expected, actual), (actual, []))
        for changed in (actual.replace(' nth_2 ', ' bad_2 ', 1), actual.replace(rows[0], rows[0].replace('\n', '\r\n'), 1)):
            self.assertNotEqual(compare_window_trace(self.source, self.expected, changed)[0], self.expected)
        changed, _ = compare_window_trace(self.source, self.expected, actual + 'ERROR: unrelated\n')
        self.assertFalse(compare(self.source, self.expected, changed)[0]['equal_after_permitted_order'])


if __name__ == '__main__':
    unittest.main()

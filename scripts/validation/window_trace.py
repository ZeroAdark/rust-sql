"""Prove pinned window outputs using one consistent order per window sort key.

Frame bounds cost O(1) for ROWS/GROUPS and O(log n) for RANGE. Exclusion leaves
at most three intervals. Partial rank outputs get a concrete full-order witness;
no factorial search or independent per-cell choice is used. This is deliberately
limited to the source-bound fixtures, including their date/interval domains.
"""
import ast
from bisect import bisect_left, bisect_right
from collections import Counter, defaultdict
from datetime import date, datetime
from functools import lru_cache
import hashlib
import json
from pathlib import Path
import re
from zoneinfo import ZoneInfo

from compare_regression import columns, find_anchor, physical_lines, statements, table


ROOT = Path(__file__).resolve().parents[2]
FIXTURES = ROOT / 'crates/postgres-18.6-reference/src/test/regress'
POLICY = ROOT / 'regress/engine-profile/policy/window-trace.json'


def digest(text):
    return hashlib.sha256(text.encode(errors='surrogateescape')).hexdigest()


def integer(text):
    number = int(text)
    if str(number) != text:
        raise ValueError('noncanonical integer')
    return number


@lru_cache(maxsize=1)
def policy():
    value = json.loads(POLICY.read_text())
    if value['version'] != 1:
        raise ValueError('unsupported window trace policy')
    return value


def values(sql):
    # Only the pinned, simple INSERT VALUES literals reach this parser.
    body = re.split(r'\bvalues\b', sql, maxsplit=1, flags=re.I)[1].strip().removesuffix(';')
    return ast.literal_eval('[' + body + ']')


@lru_cache(maxsize=1)
def inputs():
    rule = policy()
    source = (FIXTURES / 'sql/window.sql').read_text()
    data = (FIXTURES / 'data/tenk.data').read_text()
    setup = (FIXTURES / 'sql/test_setup.sql').read_text()
    if (digest(source) != rule['source_sha256'] or digest(data) != rule['data_lf_sha256']
            or digest(setup) != rule['setup_lf_sha256']):
        raise ValueError('window fixture input drift')
    commands = {line: sql for line, _, sql in statements(source)}
    employees = values(commands[rule['employee_insert_line']])
    datetimes = values(commands[rule['datetime_insert_line']])
    tenk = [line.split('\t') for line in data.splitlines()]
    # Each entry maps the complete projected input to (sort value, function value).
    result = {
        'tenk_unique2': [((str(int(r[4])), str(int(r[3]))), int(r[4]), int(r[3]))
                         for r in tenk if int(r[1]) < 10],
        'tenk_unique1': [((str(int(r[0])), str(int(r[3]))), int(r[3]), int(r[0]))
                         for r in tenk if int(r[0]) < 10],
        'employees': [((str(salary), date.fromisoformat(day).strftime('%m-%d-%Y')),
                       date.fromisoformat(day).toordinal(), salary)
                      for _, _, salary, day in employees],
        'nulls': [((str(x), str(x)), x, x) for x in range(1, 6)] +
                 [(('', str(y)), None, y) for y in (42, 43)],
        'employee_rows': [(dep, str(emp), str(salary), date.fromisoformat(day).strftime('%m-%d-%Y'))
                          for dep, emp, salary, day in employees],
    }
    for kind, column in (('time', 1), ('timetz', 2), ('interval', 3),
                         ('timestamptz', 4), ('timestamp', 5)):
        records = []
        for row in datetimes:
            identity, value = row[0], row[column]
            if value in ('infinity', '-infinity'):
                key, display = float('inf') if value == 'infinity' else -float('inf'), value
            elif kind in ('time', 'timetz'):
                hour, minute = map(int, value[:5].split(':'))
                key = hour * 60 + minute
                display = f'{hour:02}:{minute:02}:00' + ('+01' if kind == 'timetz' else '')
                if kind == 'timetz' and not value.endswith(' BST'):
                    raise ValueError('unexpected fixed-offset time fixture')
            elif kind == 'interval':
                key = int(value.split()[0])
                display = f'@ {key} year' + ('s' if key != 1 else '')
            else:
                stamp = datetime.fromisoformat(value)
                # All finite inputs are October 19 at one time of day. Thus a
                # one-year bound is exactly +/-1 on this domain, including DST.
                if (stamp.month, stamp.day, stamp.hour, stamp.minute, stamp.second) != (10, 19, 10, 23, 54):
                    raise ValueError('timestamp domain changed')
                key = stamp.year
                if kind == 'timestamptz':
                    stamp = stamp.astimezone(ZoneInfo('America/Los_Angeles'))
                display = stamp.strftime('%a %b %d %H:%M:%S %Y')
                if kind == 'timestamptz':
                    display += ' ' + stamp.tzname()
            records.append(((str(identity), display), key, identity))
        result[kind] = records
    return result


def peer_groups(keys):
    groups, group_of = [], []
    start = 0
    for end in range(1, len(keys) + 1):
        if end == len(keys) or keys[end] != keys[start]:
            group_of.extend([len(groups)] * (end - start))
            groups.append((start, end))
            start = end
    return groups, group_of


def exclude(lo, hi, current, peer, mode):
    if mode not in ('NO OTHERS', 'CURRENT ROW', 'GROUP', 'TIES'):
        raise ValueError('unknown frame exclusion')
    if lo >= hi:
        return []
    if mode == 'NO OTHERS':
        return [(lo, hi)]
    cut_lo, cut_hi = (current, current + 1) if mode == 'CURRENT ROW' else peer
    intervals = [(lo, min(hi, cut_lo))]
    if mode == 'TIES' and lo <= current < hi:
        intervals.append((current, current + 1))
    intervals.append((max(lo, cut_hi), hi))
    return [(a, b) for a, b in intervals if a < b]


def frame_values(records, rule):
    """Return projected window values for a concrete, already ordered input."""
    direction = -1 if rule.get('descending') else 1
    keys = [None if row[1] is None else direction * row[1] for row in records]
    ordered_keys = [(0, 0) if key is None else (1, key) for key in keys]
    if any(a > b for a, b in zip(ordered_keys, ordered_keys[1:])):
        raise ValueError('window input keys are not ordered')
    groups, group_of = peer_groups(keys)
    numeric = [key for key in keys if key is not None]
    null_count = len(keys) - len(numeric)
    n, output = len(records), []
    for i, record in enumerate(records):
        peer = groups[group_of[i]]
        computed = []
        for function in rule['functions']:
            name = function['name']
            if name in ('lead', 'lag'):
                j = i + (1 if name == 'lead' else -1)
                computed.append(records[j][2] if 0 <= j < n else None)
                continue
            mode, lower, upper, exclusion = function['frame']
            if mode == 'ROWS':
                lo = 0 if lower is None else max(0, min(n, i + lower))
                hi = n if upper is None else max(0, min(n, i + upper + 1))
            elif mode == 'GROUPS':
                def boundary(offset, end):
                    group = group_of[i] + offset
                    return 0 if group < 0 else n if group >= len(groups) else groups[group][int(end)]
                lo = 0 if lower is None else boundary(lower, False)
                hi = n if upper is None else boundary(upper, True)
            elif mode == 'RANGE':
                key = keys[i]
                lo = 0 if lower is None else peer[0] if key is None else null_count + bisect_left(numeric, key + lower)
                if upper == 'one_year':
                    day = date.fromordinal(key)
                    bound = day.replace(year=day.year + 1).toordinal()
                else:
                    bound = None if upper is None or key is None else key + upper
                hi = n if upper is None else peer[1] if key is None else null_count + bisect_right(numeric, bound)
            else:
                raise ValueError('unknown frame mode')
            intervals = exclude(lo, hi, i, peer, exclusion)
            if name == 'last':
                value = records[intervals[-1][1] - 1][2] if intervals else None
            else:
                index = 0 if name == 'first' else 1 if name == 'nth2' else None
                if index is None:
                    raise ValueError('unknown window function')
                value = None
                for a, b in intervals:
                    if index < b - a:
                        value = records[a + index][2]
                        break
                    index -= b - a
            computed.append(value)
        output.append(computed)
    return output


def complete_rank_orders(employee_rows, selected, column, descending=False):
    """Fill every omitted row's rank while respecting all observed assignments."""
    partitions = defaultdict(list)
    for row in employee_rows:
        partitions[row[0]].append(row)
    assignments = {row[1]: integer(row[column]) for row in selected}
    orders = {}
    for dep, records in partitions.items():
        records.sort(key=lambda row: datetime.strptime(row[3], '%m-%d-%Y').date(), reverse=descending)
        groups, _ = peer_groups([row[3] for row in records])
        ordered = [None] * len(records)
        for lo, hi in groups:
            pending = []
            for row in records[lo:hi]:
                rank = assignments.get(row[1])
                if rank is None:
                    pending.append(row)
                elif not lo < rank <= hi or ordered[rank - 1] is not None:
                    raise ValueError('inconsistent rank assignment')
                else:
                    ordered[rank - 1] = row
            missing = iter(pending)
            for i in range(lo, hi):
                if ordered[i] is None:
                    ordered[i] = next(missing)
        orders[dep] = ordered
    return orders


def prove_rank(rule, cells, data):
    employees = data['employee_rows']
    by_id = {row[1]: row for row in employees}
    if rule['kind'] == 'partition_representative':
        sizes = Counter(row[0] for row in employees)
        eligible = {dep for dep, size in sizes.items() if size <= 3}
        if Counter(row[0] for row in cells) != Counter(eligible):
            raise ValueError('missing or duplicate eligible partition')
        for row in cells:
            if tuple(row[:4]) != by_id.get(row[1]) or list(map(integer, row[4:])) != [sizes[row[0]], 1, sizes[row[0]], sizes[row[0]], 1]:
                raise ValueError('invalid partition representative or counters')
        return {'representatives': {row[0]: integer(row[1]) for row in cells}}
    if rule['kind'] == 'partition_ranks':
        if Counter(row[0] for row in cells) != Counter(by_id.keys()):
            raise ValueError('missing or duplicate ranked employee')
        selected = []
        for row in cells:
            employee = by_id[row[0]]
            if row[1] != employee[0]:
                raise ValueError('changed employee partition')
            selected.append([*employee, row[2]])
        orders = complete_rank_orders(employees, selected, 4)
        actual = {row[0]: row for row in cells}
        for dep, ordered in orders.items():
            groups, group_of = peer_groups([row[3] for row in ordered])
            for i, employee in enumerate(ordered):
                lo, hi = groups[group_of[i]]
                if list(map(integer, actual[employee[1]][2:])) != [i + 1, lo + 1, hi - lo]:
                    raise ValueError('rank, row_number or peer count changed')
        return {'orders': {dep: [integer(row[1]) for row in rows] for dep, rows in orders.items()}}
    if rule['kind'] != 'opposite_filtered_ranks':
        raise ValueError('unknown rank proof')
    if len({row[1] for row in cells}) != len(cells) or any(tuple(row[:4]) != by_id.get(row[1]) for row in cells):
        raise ValueError('changed or duplicate employee')
    ascending = complete_rank_orders(employees, cells, 4)
    descending = complete_rank_orders(employees, cells, 5, True)
    reverse_rank = {row[1]: i + 1 for ordered in descending.values() for i, row in enumerate(ordered)}
    expected = []
    for ordered in ascending.values():
        for i, employee in enumerate(ordered):
            last = reverse_rank[employee[1]]
            if i == 0 or last == 1:
                expected.append((*employee, str(i + 1), str(last)))
    if Counter(map(tuple, cells)) != Counter(expected):
        raise ValueError('filter membership or surviving rank changed')
    return {'ascending': {dep: [integer(row[1]) for row in rows] for dep, rows in ascending.items()},
            'descending': {dep: [integer(row[1]) for row in rows] for dep, rows in descending.items()}}


def prove(rule, cells, data):
    try:
        if any(len(row) != len(rule['columns']) for row in cells):
            return None
        if rule['kind'] != 'frame':
            return prove_rank(rule, cells, data)
        dataset = data[rule['dataset']]
        projected = [tuple(row[i] for i in rule['input_columns']) for row in cells]
        if Counter(projected) != Counter(row[0] for row in dataset):
            return None
        lookup = {row[0]: row for row in dataset}
        records = [lookup[row] for row in projected]
        expected = frame_values(records, rule)
        actual = [[None if row[f['column']] == '' else integer(row[f['column']])
                   for f in rule['functions']] for row in cells]
        if expected != actual:
            return None
        return {'order': [list(row) for row in projected]}
    except (ValueError, KeyError, IndexError, TypeError, StopIteration):
        return None


def compare_window_trace(source, expected, actual):
    spec = policy()
    if digest(source) != spec['source_sha256'] or digest(expected) != spec['expected_sha256']:
        return actual, []
    data = inputs()
    rules = {rule['sql_line']: rule for rule in spec['statements']}
    e, a = physical_lines(expected, keepends=True), physical_lines(actual, keepends=True)
    el, al = physical_lines(expected), physical_lines(actual)
    ep = ap = 0
    proofs = []
    for line, anchor, sql in statements(source):
        ei, ai = find_anchor(el, anchor, ep), find_anchor(al, anchor, ap)
        if ei is None or ai is None:
            break
        ep, ap = ei + len(anchor), ai + len(anchor)
        rule = rules.get(line)
        if rule is None or sql != rule['sql']:
            continue
        et, at = table(el, ep), table(al, ap)
        if not et or not at:
            continue
        ee, er = et
        ae, ar = at
        labels, spans = columns(el[ep], el[ep + 1])
        if (labels != rule['columns'] or e[ep:ep + 2] != a[ap:ap + 2] or e[ee] != a[ae]
                or any(not row.isascii() or not row.endswith('\n') or row.endswith('\r\n')
                       for row in e[ep:ee + 1] + a[ap:ae + 1])
                or any(len(row) > spans[-1][1] or any(row[b:b + 1] != '|' for _, b in spans[:-1]) for row in er + ar)):
            continue
        cell = lambda row: [row[lo:hi].strip(' ') for lo, hi in spans]
        before = prove(rule, [cell(row) for row in er], data)
        after = prove(rule, [cell(row) for row in ar], data)
        if er != ar and before is not None and after is not None:
            proofs.append({'kind': 'window_order_trace', 'sql_line': line, 'sql_sha256': digest(sql),
                           'rows': len(ar), 'expected_table_sha256': digest(''.join(e[ep:ee + 1])),
                           'actual_table_sha256': digest(''.join(a[ap:ae + 1])),
                           'native_witness': before, 'candidate_witness': after})
            a[ap + 2:ae] = e[ep + 2:ee]
        ep, ap = ee + 1, ae + 1
    return ''.join(a), proofs

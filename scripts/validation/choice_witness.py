"""Prove pinned SQL choices directly against their captured result tables.

This is not a general geometric/JSON normalizer. Unknown source or SQL hashes,
finite-distance reordering, changed ordinary values, duplicate loss and primary errors remain
strict. Each proof uses expected O(rows) work and at most O(rows) storage.
The separate aggregate-strategy witness handles one deliberately inconsistent
regression helper; it never grants an exception to ordinary aggregate results.
The worker-context witness permits only two pinned execution-site omissions,
preserving the complete primary message and SQL-function context.
"""
from collections import Counter
from functools import lru_cache
import hashlib
import json
from pathlib import Path

from compare_regression import columns, find_anchor, physical_lines, statements, table
from aggregate_strategy import compare_aggregate_strategy
from worker_context import compare_worker_context


POLICY = Path(__file__).resolve().parents[2] / "regress/engine-profile/policy/choices.json"
INFINITE_POINTS = {"(1e+300,Infinity)", "(Infinity,1e+300)"}
RADIUS_THREE_CIRCLES = {"<(1,2),3>", "<(5,1),3>"}
REPRESENTATIVES = {(3, 2), (3, 1), (1, 1), (1, 4), (5, 3), (5, 1)}
BARS = Counter([None, 3, 1, None, None, 5, 2, 4, None, 5, 4])


def digest(text):
    return hashlib.sha256(text.encode(errors="surrogateescape")).hexdigest()


@lru_cache(maxsize=1)
def policy():
    data = json.loads(POLICY.read_text())
    if data["version"] != 1:
        raise ValueError("unsupported SQL choice policy version")
    return data["sources"]


def integer(text):
    value = int(text)
    if str(value) != text:
        raise ValueError("noncanonical integer cell")
    return value


def peer_key(kind, cells, row):
    if kind == "grouping_coalesce_sum":
        # These two projected int4 columns are the complete ORDER BY input.
        # NULL is displayed as an empty cell; no rounded numeric is involved.
        values = [integer(value) if value else 0 for value in cells[:2]]
        if any(not -(2 ** 31) <= value < 2 ** 31 for value in values):
            raise ValueError("non-int4 grouping key")
        total = sum(values)
        if not -(2 ** 31) <= total < 2 ** 31:
            raise ValueError("overflowing grouping sort expression")
        return ("integer-sum", total)
    if kind == "infinite_point_distance" and cells[0] in INFINITE_POINTS:
        # Both distances from the fixed finite point (0,1) are exactly Infinity.
        return ("infinity",)
    if (kind == "circle_special_distance" and cells[0] in RADIUS_THREE_CIRCLES
            and cells[1] in INFINITE_POINTS | {"(NaN,NaN)"} and cells[2] in {"Infinity", "NaN"}):
        # Same radius means identical area; keeping the entire point identical
        # also preserves its projected x coordinate (the third ORDER BY key).
        return ("circle-peer", cells[2], cells[1])
    return ("exact", row)  # Never infer equality from rounded finite output.


def same_peer_groups(kind, expected, actual, cell):
    if Counter(expected) != Counter(actual):
        return False
    start = 0
    while start < len(expected):
        key = peer_key(kind, cell(expected[start]), expected[start])
        end = start + 1
        while end < len(expected) and peer_key(kind, cell(expected[end]), expected[end]) == key:
            end += 1
        if (any(peer_key(kind, cell(row), row) != key for row in actual[start:end]) or
                Counter(expected[start:end]) != Counter(actual[start:end])):
            return False
        start = end
    return True


def valid_representatives(rows, cell):
    values = [tuple(integer(value) for value in cell(row)) for row in rows]
    return (Counter(row[2] for row in values) == Counter([1, 2, 3]) and
            all(row[:2] in REPRESENTATIVES for row in values))


def valid_partition_arrays(rows, cell):
    bars, seen = [], {}
    for row in rows:
        bar_text, array_text = cell(row)
        bar = None if bar_text == "" else integer(bar_text)
        bars.append(bar)
        if bar is None:
            if array_text != "":
                return False
            continue
        array = json.loads(array_text)
        if not isinstance(array, list) or any(type(item) is not int for item in array):
            return False
        key = bar % 2
        if (json.dumps(array) != array_text or Counter(array) != Counter([3, 5, 5] if key else [4, 4])
                or (key in seen and seen[key] != array)):
            return False
        seen[key] = array
    return Counter(bars) == BARS


def prove(kind, expected, actual, spans):
    if not spans or any(not row.isascii() or len(row) > spans[-1][1] or
                        any(row[end:end + 1] != "|" for _, end in spans[:-1])
                        for row in expected + actual):
        return False
    cell = lambda row: [row[a:b].strip() for a, b in spans]
    try:
        if kind in {"infinite_point_distance", "circle_special_distance", "grouping_coalesce_sum"}:
            return same_peer_groups(kind, expected, actual, cell)
        if kind == "srf_distinct_representative":
            return valid_representatives(expected, cell) and valid_representatives(actual, cell)
        if kind == "json_full_partition":
            return valid_partition_arrays(expected, cell) and valid_partition_arrays(actual, cell)
    except (ValueError, TypeError, IndexError):
        return False
    raise ValueError("unrecognized SQL choice proof")


def eol(line):
    return "crlf" if line.endswith("\r\n") else "lf" if line.endswith("\n") else "none"


def compare_choices(source, expected, actual):
    actual, strategy_proofs = compare_aggregate_strategy(source, expected, actual)
    actual, worker_proofs = compare_worker_context(source, expected, actual)
    strategy_proofs.extend(worker_proofs)
    entry = policy().get(digest(source))
    if entry is None or entry["expected_sha256"] != digest(expected):
        return actual, strategy_proofs
    e, a = physical_lines(expected, keepends=True), physical_lines(actual, keepends=True)
    el, al = physical_lines(expected), physical_lines(actual)
    ep = ap = 0
    proofs = strategy_proofs
    for line, anchor, sql in statements(source):
        ei, ai = find_anchor(el, anchor, ep), find_anchor(al, anchor, ap)
        if ei is None or ai is None:
            break
        ep, ap = ei + len(anchor), ai + len(anchor)
        rule = entry["statements"].get(digest(sql))
        if rule is None:
            continue
        et, at = table(el, ep), table(al, ap)
        if not et or not at:
            continue
        ee, er = et
        ae, ar = at
        labels, spans = columns(el[ep], el[ep + 1])
        if (labels != rule["columns"] or e[ep:ep + 2] != a[ap:ap + 2] or e[ee] != a[ae]
                or Counter(map(eol, e[ep + 2:ee])) != Counter(map(eol, a[ap + 2:ae]))):
            continue
        if er != ar and prove(rule["kind"], er, ar, spans):
            proofs.append({"sql_line": line, "sql_sha256": digest(sql), "kind": rule["kind"],
                           "rows": len(er), "expected_table_sha256": digest("".join(e[ep:ee + 1])),
                           "actual_table_sha256": digest("".join(a[ap:ae + 1]))})
            a[ap + 2:ae] = e[ep + 2:ee]
        ep, ap = ee + 1, ae + 1
    return "".join(a), proofs

"""Witness one deliberately non-equivalent regression aggregate's serial result.

This does not admit differing results for ordinary aggregates. The full fixture,
native expectation, statement occurrence, input file and preceding plan are bound.
The fixture's NULL combiner cannot represent the union of its non-NULL sum states.
Input hashing and summation take O(rows) time and O(1) auxiliary space; transcript
matching is a forward pass. No runtime query behavior is changed.
"""
from functools import lru_cache
import hashlib
import json
from pathlib import Path

from compare_regression import columns, find_anchor, physical_lines, statements, table


ROOT = Path(__file__).resolve().parents[2]
POLICY = ROOT / "regress/engine-profile/policy/aggregate-strategy.json"
DATA = ROOT / "crates/postgres-18.6-reference/src/test/regress/data/tenk.data"
SETUP = ROOT / "crates/postgres-18.6-reference/src/test/regress/sql/test_setup.sql"


def digest(text):
    return hashlib.sha256(text.encode(errors="surrogateescape")).hexdigest()


@lru_cache(maxsize=1)
def policy():
    result = json.loads(POLICY.read_text())
    if result["version"] != 1:
        raise ValueError("unsupported aggregate strategy policy")
    return result


def fixture_sum(path, expected_hash):
    hasher, total, count = hashlib.sha256(), 0, 0
    with path.open() as source:
        for line in source:
            hasher.update(line.encode())  # Universal newlines match the pinned LF input.
            cells = line.rstrip("\n").split("\t")
            if len(cells) != 16:
                raise ValueError("changed tenk schema")
            value = int(cells[6])  # hundred, in the source-bound tenk1 definition
            if not 0 <= value < 100 or str(value) != cells[6]:
                raise ValueError("changed hundred input")
            total += value
            count += 1
    if hasher.hexdigest() != expected_hash or count != 10000:
        raise ValueError("changed tenk fixture")
    return total


def scalar_table(value):
    text = "" if value is None else str(value)
    width = max(4, len(text)) + 2
    return ["balk".center(width) + "\n", "-" * width + "\n",
            " " + text.rjust(width - 2) + "\n", "(1 row)\n"]


def compare_aggregate_strategy(source, expected, actual):
    rule = policy()
    if digest(source) != rule["source_sha256"] or digest(expected) != rule["expected_sha256"]:
        return actual, []
    el, al = physical_lines(expected), physical_lines(actual)
    e, a = physical_lines(expected, keepends=True), physical_lines(actual, keepends=True)
    ep = ap = occurrence = 0
    previous_plan = None
    for line, anchor, sql in statements(source):
        ei, ai = find_anchor(el, anchor, ep), find_anchor(al, anchor, ap)
        if ei is None or ai is None:
            return actual, []
        ep, ap = ei + len(anchor), ai + len(anchor)
        if sql == rule["explain"]:
            previous_plan = None
            et, at = table(el, ep), table(al, ap)
            if et and at and columns(el[ep], el[ep + 1])[0] == ["QUERY PLAN"] and columns(al[ap], al[ap + 1])[0] == ["QUERY PLAN"]:
                before, after = et[1], at[1]
                if (before and before[0].strip() == "Finalize Aggregate" and after and
                        after[0].strip() == "Aggregate" and not any(
                            word in row for row in after for word in ("Partial", "Finalize", "Gather"))):
                    previous_plan = (et[0] + 1, at[0] + 1, ep, ap)
            continue
        if sql != rule["sql"]:
            previous_plan = None
            continue
        occurrence += 1
        if occurrence != rule["occurrence"] or line != rule["sql_line"] or previous_plan is None:
            previous_plan = None
            continue
        pe, pa, plan_e, plan_a = previous_plan
        # Nothing may intervene between the reviewed plan and the query echo.
        if e[pe:ei] != ["\n"] or a[pa:ai] != ["\n"]:
            return actual, []
        et, at = table(el, ep), table(al, ap)
        if not et or not at:
            return actual, []
        ee, ae = et[0] + 1, at[0] + 1
        if digest(SETUP.read_text()) != rule["setup_lf_sha256"]:
            raise ValueError("changed tenk setup")
        total = fixture_sum(DATA, rule["data_lf_sha256"])
        if e[ep:ee] != scalar_table(None):
            raise ValueError("changed native aggregate fixture result")
        if a[ap:ae] != scalar_table(total):
            raise ValueError("serial aggregate fixture does not match its input sum")
        proof = {"kind": "deliberately_non_equivalent_aggregate_serial_result", "sql_line": line,
                 "sql_sha256": digest(sql), "occurrence": occurrence, "rows": 1,
                 "serial_sum": total, "data_lf_sha256": rule["data_lf_sha256"],
                 "setup_lf_sha256": rule["setup_lf_sha256"],
                 "expected_table_sha256": digest("".join(e[ep:ee])),
                 "actual_table_sha256": digest("".join(a[ap:ae])),
                 "native_plan_sha256": digest("".join(e[plan_e:pe])),
                 "candidate_plan_sha256": digest("".join(a[plan_a:pa]))}
        return "".join(a[:ap] + e[ep:ee] + a[ae:]), [proof]
    return actual, []

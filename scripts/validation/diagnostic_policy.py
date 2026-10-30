"""Closed, source-bound policy for reviewed diagnostic SQL, never result masks."""
from decimal import Decimal
from functools import lru_cache
import hashlib
import json
from pathlib import Path
import re

from compare_regression import FOOTER, SEPARATOR, columns, statements, table


POLICY = Path(__file__).resolve().parents[2] / "regress/engine-profile/policy/diagnostics.json"
ACTUAL = re.compile(r"\(actual rows=([0-9]+(?:\.[0-9]+)?) loops=([0-9]+|N)\)")


def json_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate JSONB object key")
        result[key] = value
    return result


def nonfinite_json(value):
    raise ValueError(f"invalid JSON constant: {value}")


def digest(value):
    return hashlib.sha256(value.encode(errors="surrogateescape")).hexdigest()


@lru_cache(maxsize=1)
def policy():
    data = json.loads(POLICY.read_text())
    if data["version"] != 1:
        raise ValueError("unsupported diagnostic policy version")
    return data["sources"]


def bindings(source):
    entry = policy().get(digest(source))
    if entry is None:
        return {}
    present = {digest(sql) for _, _, sql in statements(source)}
    if not set(entry["definitions"].values()) <= present or not set(entry["statements"]) <= present:
        raise ValueError("reviewed diagnostic definition or statement is missing")
    return entry["statements"]


def display_table(lines, start, rule):
    if start + 2 >= len(lines) or not SEPARATOR.fullmatch(lines[start + 1]):
        return None
    kind = rule["kind"]
    if kind == "memory_context_absence":
        if columns(lines[start], lines[start + 1])[0] != rule["columns"]:
            return None
        return table(lines, start)
    if lines[start].strip() != rule["header"] or "+" in lines[start + 1]:
        return None
    if kind == "text_plan":
        parsed = table(lines, start)
        return parsed if parsed and parsed[1] and all(row.startswith(" ") for row in parsed[1]) else None
    if kind != "incremental_sort_json":
        raise ValueError("unrecognized diagnostic kind")
    end = start + 2
    while end < len(lines) and not FOOTER.fullmatch(lines[end]):
        if not lines[end].startswith(" "):
            return None
        end += 1
    if end >= len(lines) or FOOTER.fullmatch(lines[end])[1] != "1":
        return None
    rows = lines[start + 2:end]
    if not rows or any(not row.endswith("+") for row in rows[:-1]) or rows[-1].endswith("+"):
        return None
    text = "\n".join([row[1:-1].rstrip() for row in rows[:-1]] + [rows[-1][1:].rstrip()])
    try:
        value = json.loads(text, parse_float=Decimal, object_pairs_hook=json_object,
                           parse_constant=nonfinite_json)
    except (ValueError, TypeError):
        return None
    if (not isinstance(value, list) or not value or
            any(not isinstance(node, dict) or node.get("Node Type") != "Incremental Sort" for node in value)):
        return None
    for node in value:
        for key in ("Actual Rows", "Actual Loops"):
            count = node.get(key)
            if type(count) not in (int, Decimal) or not Decimal(count).is_finite() or count < 0:
                return None
    return end, value


def root_count(rows):
    first = rows[0]
    actual = ACTUAL.search(first)
    if actual:
        return Decimal(actual[1]), actual[2]
    if "(actual" in first:
        raise ValueError("unrecognized root instrumentation; review required")
    return None


def guard_change(before, after, rule):
    """Diagnostic layout may change; measured root cardinality cannot disappear."""
    kind = rule["kind"]
    if kind == "memory_context_absence":
        if after:
            raise ValueError("the reviewed allocator observation requires an empty result")
    elif kind == "text_plan":
        if root_count(before) != root_count(after):
            raise ValueError("measured root rows/loops changed")
    elif kind == "incremental_sort_json":
        counts = lambda nodes: [(node["Actual Rows"], node["Actual Loops"]) for node in nodes]
        if counts(before) != counts(after):
            raise ValueError("measured incremental-sort rows/loops changed")
    else:
        raise ValueError("unrecognized diagnostic kind")

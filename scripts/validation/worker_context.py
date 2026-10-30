"""Check two pinned worker-context omissions without masking primary errors.

One forward transcript pass binds exact source, native expectation and statement
occurrences. Only the final worker stack entry may differ. SQLSTATE is verified
by the companion live protocol probe, since default psql output omits that field.
"""
from collections import Counter
from functools import lru_cache
import hashlib
import json
from pathlib import Path

from compare_regression import find_anchor, physical_lines, statements


POLICY = Path(__file__).resolve().parents[2] / "regress/engine-profile/policy/worker-context.json"


def digest(text):
    return hashlib.sha256(text.encode(errors="surrogateescape")).hexdigest()


@lru_cache(maxsize=1)
def policy():
    value = json.loads(POLICY.read_text())
    if value["version"] != 1:
        raise ValueError("unsupported worker context policy")
    return value


def compare_worker_context(source, expected, actual):
    entry = policy()
    if digest(source) != entry["source_sha256"] or digest(expected) != entry["expected_sha256"]:
        return actual, []
    rules = {(rule["sql"], rule["occurrence"]): rule for rule in entry["statements"]}
    if len(rules) != len(entry["statements"]):
        raise ValueError("duplicate worker context binding")
    e, a = physical_lines(expected, keepends=True), physical_lines(actual, keepends=True)
    el, al = physical_lines(expected), physical_lines(actual)
    ep = ap = 0
    pending, occurrences, patches, proofs = None, Counter(), [], []

    def finish(eend, aend):
        if pending is None:
            return
        estart, astart, rule = pending
        before, after = e[estart:eend], a[astart:aend]
        omitted = rule["omitted_context"]
        if omitted not in {"CONTEXT:  parallel worker\n", "parallel worker\n"}:
            raise ValueError("unrecognized worker stack entry")
        if (not before or not before[0].startswith("ERROR:  ") or before.count(omitted) != 1 or
                sum(line.startswith("ERROR:") for line in before) != 1):
            return
        index = before.index(omitted)
        if any(line.strip() for line in before[index + 1:]):
            return
        if index < 1 or after != before[:index] + before[index + 1:]:
            return
        patches.append((astart, aend, before))
        proofs.append({"kind": "postgres_worker_error_context", "sql_line": rule["sql_line"],
                       "sql_sha256": digest(rule["sql"]), "occurrence": rule["occurrence"],
                       "expected_error_sha256": digest("".join(before)),
                       "actual_error_sha256": digest("".join(after))})

    for line, anchor, sql in statements(source):
        ei, ai = find_anchor(el, anchor, ep), find_anchor(al, anchor, ap)
        if ei is None or ai is None:
            return actual, []
        finish(ei, ai)
        ep, ap = ei + len(anchor), ai + len(anchor)
        occurrences[sql] += 1
        rule = rules.get((sql, occurrences[sql]))
        pending = (ep, ap, rule) if rule and line == rule["sql_line"] else None
    finish(len(e), len(a))
    result, cursor = [], 0
    for start, end, replacement in patches:
        result.extend(a[cursor:start]);result.extend(replacement);cursor = end
    result.extend(a[cursor:])
    return "".join(result), proofs

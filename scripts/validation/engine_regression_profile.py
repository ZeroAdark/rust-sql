#!/usr/bin/env python3
"""Apply reviewed, source-bound EXPLAIN expectations without changing SQL results.

Profiles contain exact replacement plan tables, never regex masks. Recording is
an explicit authoring operation; verification only reads committed profiles.
Only the separately reviewed diagnostic policy can admit specific helper results
or allocator observations. Independent witnesses prove pinned SQL choices, one
deliberately non-equivalent aggregate fixture's serial result and two worker-stack
omissions. Ordinary SQL values, primary errors, SQL-function context and notices
stay untouched.
"""
import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path

from choice_witness import compare_choices
from compare_regression import ORDER, compare, find_anchor, physical_lines, statements
from diagnostic_policy import bindings, display_table, guard_change


def digest(text):
    return hashlib.sha256(text.encode(errors="surrogateescape")).hexdigest()


def read(path):
    return path.read_bytes().decode(errors="surrogateescape")


def plan_tables(source, transcript):
    """One forward pass over SQL echoes and policy-bound diagnostic tables."""
    lines = physical_lines(transcript)
    original = physical_lines(transcript, keepends=True)
    position, occurrences, found = 0, Counter(), {}
    helpers = bindings(source)
    for line, anchor, sql in statements(source):
        start = find_anchor(lines, anchor, position)
        if start is None:
            raise ValueError(f"missing SQL echo at source line {line}")
        position = start + len(anchor)
        key = digest(sql)
        occurrences[key] += 1
        tokens = ORDER.lex(sql)
        direct = tokens and tokens[0] == ("w", "explain")
        rule = {"kind": "text_plan", "header": "QUERY PLAN"} if direct else helpers.get(key)
        if rule is None:
            continue
        parsed = display_table(lines, position, rule)
        if parsed is None:
            continue
        end, rows = parsed
        found[(key, occurrences[key])] = {
            "start": position, "end": end + 1, "sql_line": line,
            "text": "".join(original[position:end + 1]),
            "rule": rule, "rows": rows,
        }
        position = end + 1
    return found


def validate_plan(text, rule=None):
    rule = rule or {"kind": "text_plan", "header": "QUERY PLAN"}
    lines = physical_lines(text)
    parsed = display_table(lines, 0, rule)
    if not parsed or parsed[0] != len(lines) - 1:
        raise ValueError("replacement must be exactly one valid diagnostic table")
    return parsed[1]


def apply(source, expected, profile):
    if profile.get("version") != 1:
        raise ValueError("unsupported profile version")
    if digest(source) != profile["source_sha256"] or digest(expected) != profile["expected_sha256"]:
        raise ValueError("SQL source or base expectation changed; review the profile again")
    plans, patches, seen = plan_tables(source, expected), [], set()
    for change in profile["plans"]:
        key = (change["sql_sha256"], change["occurrence"])
        if key in seen or key not in plans:
            raise ValueError("duplicate or non-EXPLAIN replacement")
        seen.add(key)
        original = plans[key]
        if digest(original["text"]) != change["before_sha256"]:
            raise ValueError("original EXPLAIN table changed")
        if change.get("kind", original["rule"]["kind"]) != original["rule"]["kind"]:
            raise ValueError("profile cannot change the diagnostic policy")
        rows = validate_plan(change["after"], original["rule"])
        guard_change(original["rows"], rows, original["rule"])
        patches.append((original["start"], original["end"], change["after"]))
    lines, result, cursor = physical_lines(expected, keepends=True), [], 0
    for start, end, replacement in sorted(patches):
        result.extend(lines[cursor:start])
        result.append(replacement)
        cursor = end
    result.extend(lines[cursor:])
    return "".join(result)


def propose(source, expected, actual, reason):
    before, after = plan_tables(source, expected), plan_tables(source, actual)
    profile = {"version": 1, "reason": reason, "source_sha256": digest(source),
               "expected_sha256": digest(expected), "plans": []}
    for key, original in before.items():
        candidate = after.get(key)
        if candidate is not None and candidate["text"] != original["text"]:
            profile["plans"].append({"sql_sha256": key[0], "occurrence": key[1],
                                     "sql_line": original["sql_line"],
                                     "kind": original["rule"]["kind"],
                                     "before_sha256": digest(original["text"]),
                                     "after": candidate["text"]})
    checked_actual, _ = compare_choices(source, expected, actual)
    check, diff = compare(source, apply(source, expected, profile), checked_actual)
    if not check["equal_after_permitted_order"] or check["refused"]:
        raise ValueError("non-plan differences remain; profile refused\n" + diff[:400])
    return profile


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--sql", type=Path, required=True)
    ap.add_argument("--expected", type=Path, required=True)
    ap.add_argument("--actual", type=Path, required=True)
    ap.add_argument("--profile", type=Path, required=True)
    ap.add_argument("--record", action="store_true", help="author an explicit profile for review")
    ap.add_argument("--reason", help="required explanation when recording")
    ap.add_argument("--report", type=Path, required=True)
    ap.add_argument("--diff", type=Path, required=True)
    args = ap.parse_args()
    source, expected, actual = map(read, (args.sql, args.expected, args.actual))
    if args.record:
        if not args.reason:
            ap.error("--record requires --reason")
        profile = propose(source, expected, actual, args.reason)
        args.profile.parent.mkdir(parents=True, exist_ok=True)
        # Never overwrite a reviewed profile implicitly.
        with args.profile.open("x", encoding="utf-8", newline="\n") as output:
            output.write(json.dumps(profile, indent=2) + "\n")
    profile = json.loads(read(args.profile))
    checked_actual, proofs = compare_choices(source, expected, actual)
    report, diff = compare(source, apply(source, expected, profile), checked_actual)
    report.update(profile=str(args.profile), profile_sha256=digest(read(args.profile)),
                  reviewed_plan_tables=len(profile["plans"]), scoped_choice_proofs=proofs)
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.diff.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(report, indent=2) + "\n")
    args.diff.write_text(diff, errors="surrogateescape")
    print(json.dumps({k: v for k, v in report.items() if k != "permitted_order_changes"}))
    return 0 if report["equal_after_permitted_order"] and not report["refused"] else 1


if __name__ == "__main__":
    raise SystemExit(main())

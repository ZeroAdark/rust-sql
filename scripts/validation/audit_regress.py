#!/usr/bin/env python3
"""Locate non-plan expectation differences for manual investigation.

This diagnostic is deliberately not a pass/fail compatibility oracle. It elides
only complete psql QUERY PLAN tables, preserving their SQL and all other output.
Raw pg_regress results remain the authoritative byte comparison.
"""
import argparse
import difflib
import json
from pathlib import Path
import re


def without_plans(text):
    lines, result, i, count = text.splitlines(), [], 0, 0
    while i < len(lines):
        if lines[i].strip() == "QUERY PLAN" and i + 1 < len(lines) and re.fullmatch(r"-+", lines[i + 1].strip()):
            end = next((j for j in range(i + 2, len(lines)) if re.fullmatch(r"\(\d+ rows?\)", lines[j])), None)
            if end is not None:
                result.append("<PLAN OUTPUT RETAINED IN RAW DIFF>")
                i, count = end + 1, count + 1
                continue
        result.append(lines[i])
        i += 1
    return result, count


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--expected", type=Path, required=True)
    ap.add_argument("--actual", type=Path, required=True)
    ap.add_argument("--output", type=Path, required=True)
    ap.add_argument("--summary", type=Path, required=True, help="pg_regress regression.out (respects alternate expectations)")
    args = ap.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    summary = []
    failed = set(re.findall(r"^not ok\s+\d+\s+[+-]\s+(\S+)", args.summary.read_text(), re.M))
    for actual in sorted(args.actual.glob("*.out")):
        if actual.stem not in failed:
            continue
        expected = args.expected / actual.name
        if not expected.exists():
            continue
        e, a = expected.read_text(errors="surrogateescape"), actual.read_text(errors="surrogateescape")
        if e == a:
            continue
        en, ne = without_plans(e)
        an, na = without_plans(a)
        diff = "\n".join(difflib.unified_diff(en, an, fromfile=str(expected), tofile=str(actual), lineterm=""))
        (args.output / f"{actual.stem}.diff").write_text(diff + "\n", errors="surrogateescape")
        summary.append({"case": actual.stem, "expected_plans": ne, "actual_plans": na,
                        "other_diff_lines": len(diff.splitlines())})
    (args.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Compare every scheduled result with native PostgreSQL and reviewed plan profiles.

The original PostgreSQL verdict is retained. Profiles never grow automatically,
and a missing result, changed native control or unreviewed difference fails.
"""
import argparse
import json
from pathlib import Path

from compare_regression import compare
from engine_regression_profile import apply, digest, read
from choice_witness import compare_choices
from choice_witness import POLICY as CHOICE_POLICY
from diagnostic_policy import POLICY as DIAGNOSTIC_POLICY
from diagnostic_policy import STATISTIC_KINDS


def audit(fixtures, reference, candidate, profiles, output):
    cases = [case for line in read(fixtures / "parallel_schedule").splitlines()
             if line.startswith("test:") for case in line.split()[1:]]
    if not cases or len(cases) != len(set(cases)):
        raise ValueError("empty or duplicate regression schedule")
    unknown = {p.stem for p in profiles.glob("*.json")} - set(cases)
    if unknown:
        raise ValueError(f"profiles not in the schedule: {sorted(unknown)}")
    results = []
    output.mkdir(parents=True, exist_ok=True)
    for case in cases:
        result = {"case": case, "passed": False}
        try:
            source = read(fixtures / "sql" / f"{case}.sql")
            native, actual = read(reference / f"{case}.out"), read(candidate / f"{case}.out")
            choices = [fixtures / "expected" / f"{case}.out"]
            choices += sorted((fixtures / "expected").glob(f"{case}_[0-9].out"))
            base = next((p for p in choices if p.exists() and read(p) == native), None)
            if base is None:
                raise ValueError("native control does not match a pinned PostgreSQL expectation")
            result.update(base_expectation=base.name, source_sha256=digest(source),
                          native_sha256=digest(native), candidate_sha256=digest(actual),
                          raw_equal=native == actual, reviewed_plan_tables=0, reviewed_statistic_tables=0)
            expected = native
            # PostgreSQL supplies whole-file alternatives for optional features
            # (XML, compression, NUMA, platform collations). An exact match is
            # already an upstream-approved result, even if the control build
            # selected a different feature branch. Never combine alternatives.
            alternative = next((p for p in choices if p.exists() and read(p) == actual), None)
            result["upstream_exact"] = alternative is not None
            if alternative is not None:
                result["candidate_expectation"] = alternative.name
                expected = read(alternative)
            profile_path = profiles / f"{case}.json"
            if profile_path.exists():
                profile = json.loads(read(profile_path))
                expected = apply(source, native, profile)
                statistics = sum(p.get("kind") in STATISTIC_KINDS for p in profile["plans"])
                result.update(reviewed_plan_tables=len(profile["plans"]) - statistics,
                              reviewed_statistic_tables=statistics,
                              profile_sha256=digest(read(profile_path)))
            candidate_for_comparison, choice_proofs = compare_choices(source, native, actual)
            comparison, diff = compare(source, expected, candidate_for_comparison)
            comparison["scoped_choice_proofs"] = choice_proofs
            # An unparseable tail remains byte-exact in compare(); identical
            # conditional/psql output needs no SQL-order proof.
            result.update(passed=comparison["equal_after_permitted_order"],
                          permitted_row_permutations=len(comparison["permitted_order_changes"]),
                          scoped_choice_proofs=len(choice_proofs),
                          refused=comparison["refused"])
            (output / f"{case}.json").write_text(json.dumps(comparison, indent=2) + "\n")
            (output / f"{case}.diff").write_text(diff, errors="surrogateescape")
        except (OSError, ValueError, KeyError) as exc:
            result["error"] = str(exc)
        results.append(result)
    return {"passed": all(r["passed"] for r in results), "total": len(cases),
            "accepted": sum(r["passed"] for r in results),
            "unresolved": [r["case"] for r in results if not r["passed"]], "cases": results}


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--fixtures", type=Path, required=True)
    ap.add_argument("--reference-report", type=Path, required=True)
    ap.add_argument("--candidate-report", type=Path, required=True)
    ap.add_argument("--profiles", type=Path, default=Path(__file__).resolve().parents[2] / "regress/engine-profile")
    ap.add_argument("--output", type=Path, required=True)
    args = ap.parse_args()
    native, candidate = json.loads(read(args.reference_report)), json.loads(read(args.candidate_report))
    if not native["passed"]:
        raise ValueError("native control failed")
    if candidate["regression"]["returncode"] not in (0, 1):
        raise ValueError("candidate regression runner failed")
    report = audit(args.fixtures, Path(native["results"]) / "results",
                   Path(candidate["regression"]["directory"]) / "results", args.profiles, args.output)
    report.update(reference_report_sha256=digest(read(args.reference_report)),
                  candidate_report_sha256=digest(read(args.candidate_report)),
                  diagnostic_policy_sha256=digest(read(DIAGNOSTIC_POLICY)),
                  choice_policy_sha256=digest(read(CHOICE_POLICY)),
                  raw_candidate_verdict=candidate["passed"])
    (args.output / "summary.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({k: v for k, v in report.items() if k != "cases"}, indent=2))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Validate peer-sensitive window values without assuming a particular tie order.

row_number over the SAME window identifies its chosen evaluation order. Verify
that it is a permutation ordered by the SQL keys, then check every frame/value.
The independent frame oracle runs against native PostgreSQL before the fork.
"""
import argparse
import hashlib
from bisect import bisect_left, bisect_right
from collections import Counter, defaultdict
import json
from pathlib import Path
import random

import psycopg
from cluster import Cluster


BOUNDS = [(None, 0), (0, None), (-2, -1), (1, 3), (-2, 2), (None, None)]
EXCLUSIONS = ["NO OTHERS", "CURRENT ROW", "GROUP", "TIES"]
MINIMAL = [
    ("first_value_following_peer", """SELECT k,first_value(k) OVER w FROM(VALUES(0),(0),(1))t(k)
      WINDOW w AS(ORDER BY k ROWS BETWEEN 1 FOLLOWING AND UNBOUNDED FOLLOWING EXCLUDE TIES)""",
     [(0, 1), (0, 1), (1, None)]),
    ("last_value_preceding_peer", """SELECT k,last_value(k) OVER w FROM(VALUES(0),(1),(1))t(k)
      WINDOW w AS(ORDER BY k ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING EXCLUDE TIES)""",
     [(0, None), (1, 0), (1, 0)]),
    ("first_value_peers_before_head", """SELECT k,first_value(k) OVER w FROM(VALUES(0),(1),(1),(1),(2))t(k)
      WINDOW w AS(ORDER BY k ROWS BETWEEN 2 FOLLOWING AND UNBOUNDED FOLLOWING EXCLUDE TIES)""",
     [(0, 1), (1, 2), (1, 2), (1, None), (2, None)]),
    ("nth_value_following_peer", """SELECT k,nth_value(k,2) OVER w,(array_agg(k) OVER w)[2]
      FROM(VALUES(0),(0),(1),(2))t(k)
      WINDOW w AS(ORDER BY k ROWS BETWEEN 1 FOLLOWING AND UNBOUNDED FOLLOWING EXCLUDE TIES)""",
     [(0, 2, 2), (0, 2, 2), (1, None, None), (2, None, None)]),
]


def bound_text(offset, lower):
    if offset is None:
        return "UNBOUNDED PRECEDING" if lower else "UNBOUNDED FOLLOWING"
    return "CURRENT ROW" if offset == 0 else f"{abs(offset)} {'PRECEDING' if offset < 0 else 'FOLLOWING'}"


def frame_intervals(i, n, groups, group_of, numeric_keys, key, mode, lower, upper, exclusion):
    """O(1) ROWS/GROUPS bounds, O(log n) RANGE bounds; at most three intervals."""
    group = group_of[i]
    peer_lo, peer_hi = groups[group]
    if mode == "ROWS":
        lo = 0 if lower is None else max(0, min(n, i + lower))
        hi = n if upper is None else max(0, min(n, i + upper + 1))
    elif mode == "GROUPS":
        def boundary(offset, end):
            target = group + offset
            return 0 if target < 0 else n if target >= len(groups) else groups[target][int(end)]
        lo = 0 if lower is None else boundary(lower, False)
        hi = n if upper is None else boundary(upper, True)
    else:
        lo = 0 if lower is None else peer_lo if key is None else bisect_left(numeric_keys, key + lower)
        hi = n if upper is None else peer_hi if key is None else bisect_right(numeric_keys, key + upper)
    if lo >= hi:
        return []
    if exclusion == "NO OTHERS":
        return [(lo, hi)]
    cut_lo, cut_hi = (i, i + 1) if exclusion == "CURRENT ROW" else (peer_lo, peer_hi)
    result = [(lo, min(hi, cut_lo))]
    if exclusion == "TIES" and lo <= i < hi:
        result.append((i, i + 1))
    result.append((max(lo, cut_hi), hi))
    return [(a, b) for a, b in result if a < b]


def validate(rows, data, mode, lower, upper, exclusion):
    partitions = defaultdict(list)
    for record in data:
        partitions[record[1]].append(record)
    by_id = {row[0]: row for row in data}
    ordered = {p: [None] * len(records) for p, records in partitions.items()}
    seen = set()
    for row in rows:
        identity, partition, _, _, number = row[:5]
        if identity in seen or tuple(row[:4]) != by_id.get(identity):
            raise AssertionError(f"input row changed or duplicated: {row[:5]}")
        seen.add(identity)
        target = ordered[partition]
        if not 1 <= number <= len(target) or target[number - 1] is not None:
            raise AssertionError(f"invalid row_number: {row[:5]}")
        target[number - 1] = row
    if len(seen) != len(data):
        raise AssertionError("missing input rows")
    for partition, records in ordered.items():
        keys = [(r[2] is None, r[2] if r[2] is not None else 0) for r in records]
        if any(a > b for a, b in zip(keys, keys[1:])):
            raise AssertionError(f"window keys out of order in partition {partition}")
        groups, group_of = [], []
        start = 0
        for end in range(1, len(records) + 1):
            if end == len(records) or keys[end] != keys[start]:
                group_of.extend([len(groups)] * (end - start))
                groups.append((start, end))
                start = end
        values = [r[3] for r in records]
        prefix_sum, prefix_count = [0], [0]
        for value in values:
            prefix_sum.append(prefix_sum[-1] + (value or 0))
            prefix_count.append(prefix_count[-1] + (value is not None))
        numeric_keys = [r[2] for r in records if r[2] is not None]
        for i, record in enumerate(records):
            intervals = frame_intervals(i, len(records), groups, group_of, numeric_keys, record[2],
                                        mode, lower, upper, exclusion)
            length = sum(b - a for a, b in intervals)
            count = sum(prefix_count[b] - prefix_count[a] for a, b in intervals)
            total = sum(prefix_sum[b] - prefix_sum[a] for a, b in intervals) if count else None
            def nth(index):
                for a, b in intervals:
                    if index < b - a:
                        return values[a + index]
                    index -= b - a
                return None
            # Materializing arrays is linear in their output size, necessarily.
            array = [values[j] for a, b in intervals for j in range(a, b)] if length else None
            json_array = [value for value in array if value is not None] if array is not None else None
            expected = (nth(0), values[intervals[-1][1] - 1] if intervals else None,
                        nth(1), total, count, length, array, json_array)
            if tuple(record[5:]) != expected:
                raise AssertionError(f"row {record[:5]}: expected {expected}, got {tuple(record[5:])}")


def exercise(connection, rounds, seed):
    connection.execute("CREATE TABLE peer_data(id integer PRIMARY KEY,p integer,k integer,v integer)")
    checked, failures = 0, []
    # Minimal upstream bug #19732 examples. No outer ORDER BY: compare exact
    # values and multiplicities, including NULLs, without assigning a tie order.
    for name, sql, expected in MINIMAL:
        try:
            cursor = connection.execute(sql, prepare=False)
            assert [column.type_code for column in cursor.description] == [23] * len(expected[0])
            rows = cursor.fetchall()
            assert Counter(rows) == Counter(expected), f"expected {expected}, got {rows}"
        except Exception as error:
            failures.append({"name": name, "error": str(error)})
        checked += 1
    for round_number in range(rounds):
        data = [(i, i % 3, None if i % 11 == 0 else i % 7,
                 None if i % 5 == 0 else i % 19 - 9) for i in range(1, 100)]
        random.Random(seed + round_number).shuffle(data)
        connection.execute("TRUNCATE peer_data")
        with connection.cursor() as cursor:
            cursor.executemany("INSERT INTO peer_data VALUES(%s,%s,%s,%s)", data)
        for mode in ("ROWS", "GROUPS", "RANGE"):
            for lower, upper in BOUNDS:
                for exclusion in EXCLUSIONS:
                    frame = f"{mode} BETWEEN {bound_text(lower, True)} AND {bound_text(upper, False)} EXCLUDE {exclusion}"
                    sql = f"""SELECT id,p,k,v,row_number() OVER w,
                      first_value(v) OVER w,last_value(v) OVER w,nth_value(v,2) OVER w,
                      sum(v) OVER w,count(v) OVER w,count(*) OVER w,array_agg(v) OVER w,
                      json_arrayagg(v ABSENT ON NULL) OVER w
                      FROM peer_data WINDOW w AS(PARTITION BY p ORDER BY k NULLS LAST {frame})"""
                    try:
                        cursor = connection.execute(sql, prepare=False)
                        if [c.type_code for c in cursor.description] != [23, 23, 23, 23, 20, 23, 23, 23, 20, 20, 20, 1007, 114]:
                            raise AssertionError("unexpected result type OIDs")
                        validate(cursor.fetchall(), data, mode, lower, upper, exclusion)
                    except Exception as error:
                        failures.append({"seed": seed + round_number, "frame": frame, "error": str(error)})
                    checked += 1
    return {"checks": checked, "failures": failures, "passed": checked - len(failures)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--seed", type=int, default=29102026)
    parser.add_argument("--reference-patch", type=Path, help="identify a separately built patched reference; never edits vendor sources")
    args = parser.parse_args()
    if args.rounds < 1:
        parser.error("--rounds must be positive")
    report = {"passed": False, "seed": args.seed, "rounds": args.rounds, "servers": {}}
    report["reference_profile"] = "patched-postgresql" if args.reference_patch else "unmodified-postgresql"
    if args.reference_patch:
        report["reference_patch_sha256"] = hashlib.sha256(args.reference_patch.read_bytes()).hexdigest()
    try:
        for name, binaries, reference in (("postgresql", args.reference, True), ("candidate", args.candidate, False)):
            with Cluster(binaries, args.work_dir, reference=reference) as server:
                report["servers"][name] = {"cluster": str(server.root), "version": server.version}
                with (binaries / "postgres").open("rb") as binary:
                    report["servers"][name]["binary_sha256"] = hashlib.file_digest(binary, "sha256").hexdigest()
                with psycopg.connect(server.dsn, autocommit=True) as connection:
                    report["servers"][name].update(exercise(connection, args.rounds, args.seed))
        # Retain failures from either server. A reference bug does not grant a
        # general exemption to the candidate, or erase a compatibility difference.
        report["passed"] = all(not value["failures"] for value in report["servers"].values())
    except Exception as error:
        report["error"] = str(error)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())

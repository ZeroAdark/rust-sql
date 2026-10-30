#!/usr/bin/env python3
"""Focused proofs for regression rows whose SQL permits multiple answers.

Never normalizes arbitrary JSON, geometric values, or DISTINCT ON statements.
These four queries and their explicit fixture data are the entire scope.
"""
import argparse
from collections import Counter
from itertools import groupby
import json
import math
from pathlib import Path
import psycopg
from cluster import Cluster


POINTS = ["(0,0)", "(-10,0)", "(-3,4)", "(5.1,34.5)", "(-5,-12)",
          "(1e-300,-1e-300)", "(1e300,Infinity)", "(Infinity,1e300)", "(NaN,NaN)", "(10,10)", None]
CIRCLES = ["<(5,1),3>", "<(1,2),100>", "<(1,3),5>", "<(1,2),3>",
           "<(100,200),10>", "<(100,1),115>", "<(3,5),0>", "<(3,5),NaN>"]
QUERIES = {
    "create_index_peers": "SELECT f1, f1 <-> '0,1'::point AS distance FROM point_tbl ORDER BY distance",
    "geometry_peers": """SELECT c.f1,p.f1,p.f1 <-> c.f1 AS distance,area(c.f1),p.f1[0]
       FROM circle_tbl c,point_tbl p WHERE p.f1 <-> c.f1 > 0 ORDER BY distance,area(c.f1),p.f1[0]""",
    "tsrf_representative": """SELECT DISTINCT ON (g) a,b,generate_series(1,3) g
       FROM (VALUES (3,2),(3,1),(1,1),(1,4),(5,3),(5,1)) AS t(a,b)""",
    "json_partition_peers": """SELECT bar,JSON_ARRAYAGG(bar) FILTER(WHERE bar > 2) OVER(PARTITION BY foo.bar%2)
       FROM (VALUES(NULL),(3),(1),(NULL),(NULL),(5),(2),(4),(NULL),(5),(4)) foo(bar)""",
}


def float_key(value):
    # PostgreSQL float ordering: finite/Infinity < NaN < NULL (ASC NULLS LAST).
    return (2, 0) if value is None else (1, 0) if math.isnan(value) else (0, value)


def freeze(value):
    if isinstance(value, float):
        return ("float", value.hex())  # exact floats; canonical spelling of NaN
    if isinstance(value, (list, tuple)):
        return tuple(map(freeze, value))
    return value


def ordered_groups(rows, key_columns):
    def key(row):
        return tuple(float_key(row[column]) for column in key_columns)
    groups = [(key_value, Counter(freeze(row) for row in members)) for key_value, members in groupby(rows, key)]
    assert all(a[0] < b[0] for a, b in zip(groups, groups[1:])), "keys out of order or split peer group"
    return groups


def exercise(connection):
    connection.execute("SET extra_float_digits=-3")
    connection.execute("CREATE TABLE point_tbl(f1 point); CREATE TABLE circle_tbl(f1 circle)")
    with connection.cursor() as cursor:
        cursor.executemany("INSERT INTO point_tbl VALUES(%s::point)", [(p,) for p in POINTS])
        cursor.executemany("INSERT INTO circle_tbl VALUES(%s::circle)", [(c,) for c in CIRCLES])
    results = {name: connection.execute(sql).fetchall() for name, sql in QUERIES.items()}
    # Exactly one representative per SRF group; every representative must occur
    # in that group of the pre-DISTINCT result. Multiplicity and key set are strict.
    representatives = results["tsrf_representative"]
    assert Counter(row[2] for row in representatives) == Counter([1, 2, 3])
    allowed = {(3, 2), (3, 1), (1, 1), (1, 4), (5, 3), (5, 1)}
    assert all(tuple(row[:2]) in allowed for row in representatives)
    # The full-partition aggregate has no order clause. Only this query permits
    # element permutations, with exact duplicates and identical values per group.
    json_rows = results["json_partition_peers"]
    assert Counter(row[0] for row in json_rows) == Counter([None, 3, 1, None, None, 5, 2, 4, None, 5, 4])
    seen = {}
    for bar, array in json_rows:
        if bar is None:
            assert array is None
        else:
            key = bar % 2
            assert array is not None and Counter(array) == Counter([3, 5, 5] if key else [4, 4])
            assert key not in seen or seen[key] == array
            seen[key] = array
    return results


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("reference", "candidate", "work-dir", "output"):
        parser.add_argument("--" + name, required=True, type=Path)
    args = parser.parse_args()
    report, results = {"passed": False, "servers": {}, "queries": QUERIES}, {}
    try:
        for name, binaries, reference in (("postgresql", args.reference, True), ("candidate", args.candidate, False)):
            with Cluster(binaries, args.work_dir, reference=reference) as server:
                with psycopg.connect(server.dsn, autocommit=True) as connection:
                    results[name] = exercise(connection)
                report["servers"][name] = {"version": server.version, "cluster": str(server.root),
                                            "rows": {key: freeze(value) for key, value in results[name].items()}}
        for name, columns in (("create_index_peers", [1]), ("geometry_peers", [2, 3, 4])):
            assert ordered_groups(results["postgresql"][name], columns) == ordered_groups(results["candidate"][name], columns), name
        report["passed"] = True
    except Exception as error:
        report["error"] = str(error)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({key: value for key, value in report.items() if key not in ("queries", "servers")}, indent=2))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())

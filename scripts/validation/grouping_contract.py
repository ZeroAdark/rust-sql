#!/usr/bin/env python3
"""Independent grouping-set values and planner-rewrite equivalence checks."""
import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path

import psycopg
from cluster import Cluster
from explain_contract import nodes


ROWS = [(1, 1, 10), (1, 1, 11), (1, 2, 12), (1, 2, 13), (1, 3, 14),
        (2, 3, 15), (3, 3, 16), (3, 4, 17), (4, 1, 18), (4, 1, 19)]
NULL_ROWS = ROWS + [(None, None, None), (None, 2, 7), (1, None, -4), (None, 2, 7)]
SETS = [(0, 1), (0,), ()]
SELECT = "SELECT a,b,grouping(a,b),sum(v),count(*),max(v) FROM gcheck"
ORDER = " ORDER BY a,b,grouping(a,b)"


def oracle(rows, sets=SETS, having=False):
    result = []
    for grouping in sets:
        groups = {} if grouping else {(): []}
        for row in rows:
            groups.setdefault(tuple(row[i] for i in grouping), []).append(row)
        for key, members in groups.items():
            projected = dict(zip(grouping, key))
            a, b = projected.get(0), projected.get(1)
            if having and not (a is not None and b is not None and a > 1 and b > 1):
                continue
            mask = (2 if 0 not in grouping else 0) + (1 if 1 not in grouping else 0)
            values = [row[2] for row in members if row[2] is not None]
            result.append((a, b, mask, sum(values) if values else None,
                           len(members), max(values) if values else None))
    return sorted(result, key=lambda row: (row[0] is None, row[0] or 0, row[1] is None, row[1] or 0, row[2]))


def cases():
    base = oracle(ROWS)
    nullable = oracle(NULL_ROWS)
    return [
        ("partial_integer_sort", ROWS, SELECT + " GROUP BY ROLLUP(a,b) ORDER BY coalesce(a,0)+coalesce(b,0)", base, True),
        ("nullable_groups", NULL_ROWS, SELECT + " GROUP BY ROLLUP(a,b)" + ORDER, nullable, False),
        ("empty_input", [], SELECT + " GROUP BY ROLLUP(a,b)" + ORDER, oracle([]), False),
        ("duplicate_sets", NULL_ROWS, SELECT + " GROUP BY GROUPING SETS((a,b),(a,b),(a),())" + ORDER,
         oracle(NULL_ROWS, [(0, 1), (0, 1), (0,), ()]), False),
        ("having_nullable_columns", NULL_ROWS, SELECT + " GROUP BY GROUPING SETS((a,b),(a)) HAVING a>1 AND b>1" + ORDER,
         oracle(NULL_ROWS, [(0, 1), (0,)], having=True), False),
        ("window_over_groups", NULL_ROWS,
         SELECT.replace("max(v)", "max(v),(sum(sum(v)) OVER ())::bigint") + " GROUP BY ROLLUP(a,b)" + ORDER,
         [(*row, sum(r[3] for r in nullable if r[3] is not None)) for row in nullable], False),
        ("target_set_returning", [], "SELECT generate_series(1,a) AS g,a+b AS ab FROM (VALUES(1,1),(2,2)) t(a,b) GROUP BY ROLLUP(a,ab) ORDER BY 1,2",
         [(1, 2), (1, 4), (1, None), (1, None), (2, 4), (2, None)], False),
        ("constant_rollup", [], "SELECT 1 AS one GROUP BY ROLLUP(one) ORDER BY one NULLS FIRST", [(None,), (1,)], False),
        ("union_null_label", [], "SELECT * FROM (SELECT NULL::integer AS one UNION ALL SELECT 1) s ORDER BY one NULLS FIRST", [(None,), (1,)], False),
        ("union_constant_label", [], "SELECT * FROM (SELECT 2 AS one UNION ALL SELECT 1) s ORDER BY one", [(1,), (2,)], False),
    ]


def exercise(connection, mode):
    result = []
    connection.execute("SET enable_hashagg=off")
    for name, data, sql, expected, partial in cases():
        connection.execute("TRUNCATE gcheck")
        if data:
            with connection.cursor() as cursor:
                cursor.executemany("INSERT INTO gcheck VALUES(%s,%s,%s)", data)
        assert Counter(connection.execute("SELECT * FROM gcheck").fetchall()) == Counter(data)
        cursor = connection.execute(sql)
        rows = cursor.fetchall()
        types = [column.type_code for column in cursor.description]
        if partial:
            assert Counter(rows) == Counter(expected), (name, expected, rows)
            keys = [(row[0] or 0) + (row[1] or 0) for row in rows]
            assert keys == sorted(keys), (name, keys)
        else:
            assert rows == expected, (name, expected, rows)
        document = connection.execute("EXPLAIN (ANALYZE,VERBOSE,FORMAT JSON,TIMING OFF,BUFFERS OFF) " + sql).fetchone()[0][0]
        assert document["Plan"]["Actual Rows"] == len(expected) and document["Plan"]["Actual Loops"] == 1
        if not name.startswith("union_") and mode in {"off", "force"}:
            kinds = {node["Node Type"] for node in nodes(document["Plan"])}
            assert bool(kinds & {"Append", "Merge Append"}) == (mode == "force"), (name, mode, kinds)
        result.append({"name": name, "sql": sql, "types": types, "expected": expected, "rows": rows, "explain": document})
    return result


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    for name in ("reference", "candidate", "work-dir", "output"):
        ap.add_argument("--" + name, type=Path, required=True)
    args = ap.parse_args()
    report = {"passed": False, "engines": {}}
    try:
        for name, binaries in (("native", args.reference), ("candidate", args.candidate)):
            with Cluster(binaries, args.work_dir, reference=name == "native", settings=["max_parallel_workers_per_gather=0"]) as server:
                with (binaries / "postgres").open("rb") as file:
                    identity = hashlib.file_digest(file, "sha256").hexdigest()
                engine = {"cluster": str(server.root), "version": server.version, "binary_sha256": identity, "modes": {}}
                report["engines"][name] = engine
                with psycopg.connect(server.dsn, autocommit=True) as connection:
                    connection.execute("CREATE TEMP TABLE gcheck(a int,b int,v int)")
                    for mode in (["default"] if name == "native" else ["default", "off", "force"]):
                        if mode != "default":
                            connection.execute("SET pgrust.gravity_groupingsets_rewrite=" + mode)
                        engine["modes"][mode] = exercise(connection, mode)
        native = report["engines"]["native"]["modes"]["default"]
        for checks in report["engines"]["candidate"]["modes"].values():
            assert len(checks) == len(native)
            for p, c in zip(native, checks):
                assert (p["name"], p["types"]) == (c["name"], c["types"])
                if p["name"].startswith("union_"):
                    assert p["explain"]["Plan"]["Sort Key"] == c["explain"]["Plan"]["Sort Key"]
        report["passed"] = True
    except Exception as exc:
        report["error"] = str(exc)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({k: v for k, v in report.items() if k != "engines"}))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())

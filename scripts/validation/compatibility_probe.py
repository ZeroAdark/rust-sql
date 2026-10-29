#!/usr/bin/env python3
"""Compare SQL semantics and skipped-plan instrumentation with native PostgreSQL."""
import argparse
from collections import Counter
import json
from pathlib import Path
import random
import subprocess

import psycopg
from cluster import Cluster


def run(args):
    report = {"passed": False, "checks": [], "failures": []}
    with Cluster(args.reference, args.work_dir, reference=True,
                 settings=["max_parallel_workers_per_gather=0"]) as pg, Cluster(args.candidate, args.work_dir) as candidate:
        report["clusters"] = {"postgres": str(pg.root), "candidate": str(candidate.root)}
        report["versions"] = {"postgres": pg.version, "candidate": candidate.version}
        with psycopg.connect(pg.dsn, autocommit=True) as pc, psycopg.connect(candidate.dsn, autocommit=True) as cc:
            def query_check(name, sql, *, ordered=True):
                def answer(conn):
                    try:
                        cur = conn.execute(sql, prepare=False)
                        return {"types": [d.type_code for d in cur.description],
                                "rows": [tuple(repr(v) for v in row) for row in cur.fetchall()]}
                    except psycopg.Error as error:
                        return {"sqlstate": error.sqlstate, "message": error.diag.message_primary}
                p, c = answer(pc), answer(cc)
                same = p == c if ordered or "rows" not in p or "rows" not in c else p["types"] == c["types"] and Counter(p["rows"]) == Counter(c["rows"])
                if same:
                    report["checks"].append(name)
                else:
                    report["failures"].append({"name": name, "sql": sql, "postgres": p, "candidate": c})

            setup = """
            CREATE TABLE t(id integer PRIMARY KEY, k integer, v integer, label text);
            INSERT INTO t SELECT i, CASE WHEN i%11=0 THEN NULL ELSE i%7 END,
              CASE WHEN i%13=0 THEN NULL ELSE i%19-9 END, CASE WHEN i%5=0 THEN NULL ELSE 's'||(i%3) END
              FROM generate_series(1,300) g(i);
            CREATE TABLE u AS SELECT id,k,v FROM t WHERE id%3=0;
            ANALYZE t; ANALYZE u;
            CREATE SEQUENCE effects;
            PREPARE no_data_query AS SELECT nextval('effects'), generate_series(1,3);
            """
            for conn in (pc, cc):
                conn.execute(setup, prepare=False)
            queries = {
                "group_null_duplicate": "SELECT k,count(*),count(v),sum(v),min(v),max(v) FROM t GROUP BY k ORDER BY k NULLS FIRST",
                "ordered_aggregate": "SELECT k,array_agg(v ORDER BY id),string_agg(label,',' ORDER BY id) FROM t GROUP BY k ORDER BY k NULLS FIRST",
                "window_total_order": "SELECT id,row_number() OVER w,rank() OVER w,last_value(v) OVER w FROM t WINDOW w AS (PARTITION BY k ORDER BY id) ORDER BY id",
                "window_range_peers": "SELECT id,sum(v) OVER(PARTITION BY k ORDER BY v RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
                "window_rows_exclusion": "SELECT id,sum(v) OVER(ORDER BY id ROWS BETWEEN 2 PRECEDING AND 2 FOLLOWING EXCLUDE CURRENT ROW) FROM t ORDER BY id",
                "window_groups": "SELECT id,count(*) OVER(ORDER BY k GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE TIES) FROM t ORDER BY id",
                "rollup_ordered": "SELECT k,label,grouping(k,label),count(*),sum(v) FROM t GROUP BY ROLLUP(k,label) ORDER BY grouping(k,label),k NULLS FIRST,label NULLS FIRST",
                "full_join": "SELECT t.id,u.id,t.v,u.v FROM t FULL JOIN u ON t.k=u.k AND t.id=u.id ORDER BY t.id NULLS FIRST,u.id NULLS FIRST",
                "null_not_in": "SELECT id FROM t WHERE v NOT IN (SELECT v FROM u) ORDER BY id",
                "correlated_exists": "SELECT id FROM t WHERE EXISTS(SELECT 1 FROM u WHERE u.k=t.k AND u.v<t.v) ORDER BY id",
                "lateral_limit": "SELECT t.id,x.id FROM t LEFT JOIN LATERAL(SELECT id FROM u WHERE u.k=t.k ORDER BY id LIMIT 2) x ON true ORDER BY t.id,x.id",
                "union_all_duplicates": "SELECT k FROM t UNION ALL SELECT k FROM u",
                "json_window_total_order": "SELECT id,json_arrayagg(v ABSENT ON NULL) OVER(PARTITION BY k ORDER BY id ROWS BETWEEN 3 PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
                "numeric_boundary": "SELECT sum(v::numeric),avg(v::numeric),sum(v::bigint) FROM t",
                "error_division": "SELECT 1/0",
                "error_overflow": "SELECT 2147483647::integer + 1",
                "smallint_function_resolution": "SELECT * FROM generate_series(1::smallint,3::smallint)",
            }
            for name, sql in queries.items():
                query_check(name, sql, ordered=name != "union_all_duplicates")
            # Seeded variation preserves a total order, avoiding accidental tie-order expectations.
            rng = random.Random(2901026)
            for i in range(64):
                threshold, width, offset = rng.randrange(-10, 10), rng.randrange(1, 31), rng.randrange(0, 15)
                query_check(f"generated_{i}", f"SELECT id,k,sum(v) OVER(PARTITION BY k ORDER BY id ROWS BETWEEN {width} PRECEDING AND CURRENT ROW) FROM t WHERE v>={threshold} ORDER BY id LIMIT {width} OFFSET {offset}")
            for suffix, body in (("direct", "SELECT nextval('effects'),generate_series(1,3)"), ("prepared", "EXECUTE no_data_query")):
                sql = f"EXPLAIN (ANALYZE, FORMAT JSON, COSTS OFF, TIMING OFF, BUFFERS OFF, SUMMARY OFF) CREATE TABLE empty_{suffix} AS {body} WITH NO DATA"
                plans = [conn.execute(sql).fetchone()[0][0]["Plan"] for conn in (pc, cc)]
                def counters(plan):
                    return [(plan["Node Type"], plan.get("Actual Rows"), plan.get("Actual Loops"))] + [v for p in plan.get("Plans", []) for v in counters(p)]
                expected, got = map(counters, plans)
                if expected == got:
                    report["checks"].append(f"no_data_{suffix}_instrumentation")
                else:
                    report["failures"].append({"name": f"no_data_{suffix}_instrumentation", "postgres": expected, "candidate": got})
                query_check(f"no_data_{suffix}_empty", f"SELECT count(*) FROM empty_{suffix}")
            query_check("no_data_volatile_not_evaluated", "SELECT is_called FROM effects")
            # This deliberately non-associative combiner is used by the upstream parallel fixture.
            for conn in (pc, cc):
                conn.execute("CREATE FUNCTION null_combine(bigint,bigint) RETURNS bigint LANGUAGE SQL IMMUTABLE STRICT PARALLEL SAFE AS 'SELECT NULL::bigint'; CREATE AGGREGATE balk(integer)(SFUNC=int4_sum,STYPE=bigint,COMBINEFUNC=null_combine,PARALLEL=SAFE,INITCOND='0')", prepare=False)
            query_check("serial_balk_aggregate", "SELECT balk(v) FROM t")
        if args.isolation_dir:
            output = pg.root / "isolation-control"
            output.mkdir()
            testdir = args.isolation_dir.resolve()
            cmd = [str(testdir / "pg_isolation_regress"), f"--inputdir={testdir}", f"--outputdir={output}",
                   f"--bindir={Path(args.reference).resolve()}", f"--host={pg.socket}", "--port=55440",
                   "--user=rustsql", "--dbname=isolation_regression", "deadlock-parallel"]
            with (output / "run.log").open("w") as log:
                subprocess.run(cmd, cwd=testdir, env=pg.env, stdout=log, stderr=log, timeout=90)
            expected = args.overlay / "expected/deadlock-parallel.out"
            result = output / "results/deadlock-parallel.out"
            if result.read_bytes() == expected.read_bytes():
                report["checks"].append("native_postgresql_serial_isolation_matches_overlay")
            else:
                report["failures"].append({"name": "native_postgresql_serial_isolation_matches_overlay", "output": str(result)})
    report["passed"] = not report["failures"]
    return report


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--reference", type=Path, required=True)
    ap.add_argument("--candidate", type=Path, required=True)
    ap.add_argument("--work-dir", type=Path, required=True)
    ap.add_argument("--output", type=Path, required=True)
    ap.add_argument("--isolation-dir", type=Path)
    ap.add_argument("--overlay", type=Path, default=Path(__file__).resolve().parents[2] / "regress/isolation-overlay")
    args = ap.parse_args()
    report = run(args)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())

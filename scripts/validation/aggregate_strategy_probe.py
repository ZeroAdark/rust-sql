#!/usr/bin/env python3
"""Native serial/partial controls for the deliberately NULL-combining fixture."""
import argparse
from collections import defaultdict
import hashlib
import json
from pathlib import Path

import psycopg
from aggregate_strategy import DATA, ROOT, SETUP, digest, fixture_sum, policy
from cluster import Cluster
from compare_regression import statements
from explain_contract import nodes


def check(connection, evidence, name, sql, expected, types, partial=False):
    cursor = connection.execute(sql, prepare=False)
    rows = cursor.fetchall()
    actual_types = [column.type_code for column in cursor.description]
    assert rows == expected and actual_types == types, (name, rows, actual_types, expected, types)
    plan = connection.execute("EXPLAIN (ANALYZE,VERBOSE,FORMAT JSON,TIMING OFF,BUFFERS OFF) " + sql).fetchone()[0][0]
    root = plan["Plan"]
    assert root["Actual Rows"] == len(rows) and root["Actual Loops"] == 1, (name, root)
    visited = list(nodes(root))
    modes = {node.get("Partial Mode", "Simple") for node in visited if node["Node Type"] == "Aggregate"}
    if partial:
        assert {"Finalize", "Partial"} <= modes, (name, plan)
        workers = sum(node.get("Workers Launched", 0) for node in visited)
        assert workers >= 1, (name, "no real parallel worker", plan)
    else:
        assert modes == {"Simple"}, (name, plan)
    evidence.append({"name": name, "sql": sql, "types": actual_types, "values": rows, "explain": plan})


def run(args, binary, reference):
    rule = policy()
    source = (ROOT / "crates/postgres-18.6-reference/src/test/regress/sql/aggregates.sql").read_text()
    assert digest(source) == rule["source_sha256"] and digest(SETUP.read_text()) == rule["setup_lf_sha256"]
    definitions = [sql for _, _, sql in statements(source) if sql.startswith(("CREATE FUNCTION balk", "CREATE AGGREGATE balk"))]
    assert len(definitions) == 4
    total = fixture_sum(DATA, rule["data_lf_sha256"])
    input_data = DATA.read_text()
    grouped = defaultdict(int)
    for line in input_data.splitlines():
        value = int(line.split("\t")[6])
        grouped[value % 7] += value
    grouped = sorted(grouped.items())
    evidence = []
    with Cluster(binary, args.work_dir, reference=reference, settings=["autovacuum=off"]) as cluster:
        with psycopg.connect(cluster.dsn, autocommit=True) as connection:
            setup = SETUP.read_text();start = setup.index("CREATE TABLE tenk1 (")
            connection.execute(setup[start:setup.index(");", start) + 2])
            with connection.cursor().copy("COPY tenk1 FROM STDIN") as copy:
                copy.write(input_data)
            connection.execute("CREATE INDEX ON tenk1(hundred)")
            connection.execute("VACUUM ANALYZE tenk1")
            connection.execute("ALTER TABLE tenk1 SET(parallel_workers=4)")
            connection.execute("SET statement_timeout='60s'; SET parallel_setup_cost=0; SET max_parallel_workers_per_gather=0")
            small = [
                ("empty", "SELECT balk(hundred) FROM tenk1 WHERE false", [(0,)]),
                ("null_only", "SELECT balk(n) FROM (VALUES(NULL::int),(NULL::int)) v(n)", [(0,)]),
                ("signed_duplicates_and_nulls", "SELECT balk(n) FROM (VALUES(-4),(NULL),(7),(NULL),(7)) v(n)", [(10,)]),
            ]
            connection.execute("BEGIN")
            for sql in definitions[:2]:connection.execute(sql)
            check(connection, evidence, "null_transition_full", rule["sql"], [(None,)], [20])
            for name, sql, expected in small:
                check(connection, evidence, "null_transition_" + name, sql,
                      [(None,)] if name == "signed_duplicates_and_nulls" else expected, [20])
            connection.execute("ROLLBACK")
            connection.execute("BEGIN")
            for sql in definitions[2:]:connection.execute(sql)
            check(connection, evidence, "null_combiner_serial_full", rule["sql"], [(total,)], [20])
            for name, sql, expected in small:
                check(connection, evidence, "null_combiner_serial_" + name, sql, expected, [20])
            group_sql = "SELECT hundred%7 AS g,balk(hundred) FROM tenk1 GROUP BY 1 ORDER BY 1"
            check(connection, evidence, "null_combiner_serial_groups", group_sql, grouped, [23,20])
            connection.execute("SET LOCAL max_parallel_workers_per_gather=4")
            check(connection, evidence, "null_combiner_parallel_requested", rule["sql"],
                  [(None if reference else total,)], [20], partial=reference)
            connection.execute("DROP AGGREGATE balk(int4)")
            # A valid combiner must agree with serial aggregation on the same input.
            connection.execute(definitions[3].replace("balkifnull(int8, int8)", "int8pl(int8, int8)"))
            for workers in (0,4):
                connection.execute(f"SET LOCAL max_parallel_workers_per_gather={workers}")
                check(connection, evidence, f"valid_combiner_full_workers_{workers}", rule["sql"], [(total,)], [20], partial=reference and workers == 4)
                check(connection, evidence, f"valid_combiner_groups_workers_{workers}", group_sql, grouped, [23,20], partial=reference and workers == 4)
                if workers == 0:
                    for name, sql, expected in small:
                        check(connection, evidence, "valid_combiner_" + name, sql, expected, [20])
            connection.execute("ROLLBACK")
        with (Path(binary)/"postgres").open("rb") as executable:
            binary_hash = hashlib.file_digest(executable, "sha256").hexdigest()
        result = {"version": cluster.version, "cluster": str(cluster.root), "binary_sha256": binary_hash,
                  "data_lf_sha256": rule["data_lf_sha256"], "setup_lf_sha256": rule["setup_lf_sha256"], "source_sha256": digest(source), "checks": evidence}
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference-bin", type=Path, required=True)
    parser.add_argument("--candidate-bin", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    report = {"passed": False, "engines": {}}
    try:
        for name, binary, reference in (("native",args.reference_bin,True),("candidate",args.candidate_bin,False)):
            report["engines"][name] = run(args,binary,reference)
        report["passed"] = True
    except Exception as error:
        report["error"] = repr(error)
        raise
    finally:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report,indent=2) + "\n")
    print(json.dumps({"passed":True,"checks":{name:len(engine["checks"]) for name,engine in report["engines"].items()}}))


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Check truthful Gravity EXPLAIN fields and instrumented query side effects."""
import argparse
import json
from pathlib import Path
import psycopg
from cluster import Cluster


CASES = [
    ("sort", "SELECT id,k,v FROM explain_data ORDER BY v,id", "Sort", {}),
    ("incremental_sort_limit", "SELECT id,k,v FROM explain_data ORDER BY k,v,id LIMIT 20", "Incremental Sort",
     {"enable_seqscan": "off", "enable_bitmapscan": "off", "enable_sort": "off"}),
    ("hash_join", "SELECT d.id,e.id FROM explain_data d JOIN explain_data e USING(v)", "Hash",
     {"enable_mergejoin": "off", "enable_nestloop": "off"}),
    ("cte", "WITH t AS MATERIALIZED(SELECT * FROM explain_data) SELECT * FROM t", "CTE Scan", {}),
    ("window", "SELECT id,sum(v) OVER(ORDER BY id) FROM explain_data", "WindowAgg", {}),
]
UNMEASURED = {
    "Sort": {"Sort Method", "Sort Space Used", "Sort Space Type"},
    "Incremental Sort": {"Full-sort Groups", "Pre-sorted Groups"},
    "Hash": {"Hash Buckets", "Original Hash Buckets", "Hash Batches", "Original Hash Batches", "Peak Memory Usage"},
    "CTE Scan": {"Storage", "Maximum Storage"},
    "Materialize": {"Storage", "Maximum Storage"},
    "WindowAgg": {"Storage", "Maximum Storage"},
}


def nodes(root):
    yield root
    for child in root.get("Plans", []):
        yield from nodes(child)


def exercise(connection, check_metrics=True):
    connection.execute("CREATE TABLE explain_data AS SELECT i id, i%7 k, i%97 v FROM generate_series(1,1000) i")
    connection.execute("CREATE INDEX explain_data_k ON explain_data(k)")
    connection.execute("CREATE INDEX explain_data_id ON explain_data(id)")
    connection.execute("ANALYZE explain_data")
    evidence = []
    for name, sql, required_node, settings in CASES if check_metrics else []:
        with connection.transaction():
            for key, value in settings.items():
                connection.execute(f"SET LOCAL {key}={value}")
            expected_count = len(connection.execute(sql).fetchall())
            document = connection.execute("EXPLAIN (ANALYZE,BUFFERS,FORMAT JSON) " + sql).fetchone()[0][0]
            root = document["Plan"]
            visited = list(nodes(root))
            assert required_node in {node["Node Type"] for node in visited}, (name, document)
            assert root["Actual Rows"] == expected_count and root["Actual Loops"] == 1, (name, document)
            for node in visited:
                unexpected = UNMEASURED.get(node["Node Type"], set()) & node.keys()
                assert not unexpected, (name, "unmeasured fields", sorted(unexpected), node)
            evidence.append({"name": name, "rows": expected_count, "explain": document})
    # Instrumentation must not alter the number of volatile expressions evaluated
    # by a prefix-limited incremental sort. Reset the owned sequence for each run.
    with connection.transaction():
        connection.execute("CREATE SEQUENCE explain_calls")
        connection.execute("SET LOCAL enable_seqscan=off")
        connection.execute("SET LOCAL enable_bitmapscan=off")
        connection.execute("SET LOCAL enable_sort=off")
        sql = "SELECT k,v,nextval('explain_calls') FROM explain_data ORDER BY k,v,id LIMIT 20"
        connection.execute(sql).fetchall()
        normal = connection.execute("SELECT last_value,is_called FROM explain_calls").fetchone()
        connection.execute("ALTER SEQUENCE explain_calls RESTART")
        analyzed = connection.execute("EXPLAIN(ANALYZE,FORMAT JSON) " + sql).fetchone()[0][0]
        assert "Incremental Sort" in {node["Node Type"] for node in nodes(analyzed["Plan"])}, analyzed
        instrumented = connection.execute("SELECT last_value,is_called FROM explain_calls").fetchone()
        assert instrumented == normal == (20, True), (normal, instrumented)
        evidence.append({"name": "incremental_sort_volatile_limit", "sequence_calls": normal[0], "explain": analyzed})
        # One-row prefix groups expose the retired replay's minimum batch size.
        # A volatile SORT KEY counts evaluated input rows, not just output rows.
        connection.execute("ALTER SEQUENCE explain_calls RESTART")
        sql = "SELECT id FROM explain_data ORDER BY id,nextval('explain_calls') LIMIT 1"
        connection.execute(sql).fetchall()
        normal = connection.execute("SELECT last_value,is_called FROM explain_calls").fetchone()
        connection.execute("ALTER SEQUENCE explain_calls RESTART")
        analyzed = connection.execute("EXPLAIN(ANALYZE,FORMAT JSON) " + sql).fetchone()[0][0]
        assert "Incremental Sort" in {node["Node Type"] for node in nodes(analyzed["Plan"])}, analyzed
        instrumented = connection.execute("SELECT last_value,is_called FROM explain_calls").fetchone()
        assert instrumented == normal, ("volatile sort-key evaluations", normal, instrumented)
        evidence.append({"name": "incremental_sort_volatile_key", "sequence_calls": normal[0], "explain": analyzed})
    return evidence


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate", required=True, type=Path)
    parser.add_argument("--work-dir", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--volatile-only", action="store_true", help="compare ANALYZE side effects only, including on an older binary")
    args = parser.parse_args()
    report = {"passed": False, "profile": "volatile-only" if args.volatile_only else "truthful-metrics-and-effects"}
    try:
        with Cluster(args.candidate, args.work_dir) as server:
            report.update(cluster=str(server.root), version=server.version)
            with psycopg.connect(server.dsn, autocommit=True) as connection:
                report["checks"] = exercise(connection, not args.volatile_only)
        report["passed"] = True
    except Exception as error:
        report["error"] = str(error)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({key: value for key, value in report.items() if key != "checks"}, indent=2))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())

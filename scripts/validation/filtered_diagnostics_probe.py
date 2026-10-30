#!/usr/bin/env python3
"""Unmasked SQL/counter checks behind reviewed EXPLAIN and pruning helpers."""
import argparse
from collections import Counter
from decimal import Decimal, ROUND_HALF_UP
import hashlib
import json
from pathlib import Path

import psycopg
from cluster import Cluster
from explain_contract import nodes, UNMEASURED


FIXTURES = Path(__file__).resolve().parents[2] / "crates/postgres-18.6-reference/src/test/regress"
AB = [(a, b) for a in range(1, 4) for b in range(1, 4)] + [(1, 2), (3, 3)]


def digest(value):
    return hashlib.sha256(json.dumps(value, default=str, separators=(",", ":")).encode()).hexdigest()


def average(values):
    return None if not values else (Decimal(sum(values)) / len(values)).quantize(Decimal("1e-16"), rounding=ROUND_HALF_UP)


def check(connection, evidence, candidate, name, sql, expected, *, ordered=True, required=None):
    cursor = connection.execute(sql, prepare=False)
    rows = cursor.fetchall()
    types = [column.type_code for column in cursor.description]
    assert (rows == expected if ordered else Counter(rows) == Counter(expected)), (name, expected[:12], rows[:12])
    document = connection.execute("EXPLAIN (ANALYZE,VERBOSE,FORMAT JSON,TIMING OFF,BUFFERS OFF) " + sql).fetchone()[0][0]
    root = document["Plan"]
    assert root["Actual Rows"] == len(expected) and root["Actual Loops"] == 1, (name, root)
    visited = list(nodes(root))
    if required:
        assert required in {node["Node Type"] for node in visited}, (name, required, document)
    if candidate:
        for node in visited:
            assert not UNMEASURED.get(node["Node Type"], set()) & node.keys(), (name, node)
    canonical = rows if ordered else sorted(rows, key=lambda row: json.dumps(row, default=str))
    evidence.append({"name": name, "sql": sql, "types": types, "rows": len(rows),
                     "values_sha256": digest(canonical), "explain": document})


def setup_partitions(connection):
    connection.execute("CREATE TABLE ab(a int NOT NULL,b int NOT NULL) PARTITION BY LIST(a)")
    for a in range(1, 4):
        connection.execute(f"CREATE TABLE ab_a{a} PARTITION OF ab FOR VALUES IN({a}) PARTITION BY LIST(b)")
        for b in range(1, 4):
            connection.execute(f"CREATE TABLE ab_a{a}_b{b} PARTITION OF ab_a{a} FOR VALUES IN({b})")
            connection.execute(f"CREATE INDEX ON ab_a{a}_b{b}(a)")
    with connection.cursor() as cursor:
        cursor.executemany("INSERT INTO ab VALUES(%s,%s)", AB)
    connection.execute("ANALYZE ab")


def explain_checks(connection, evidence, candidate):
    setup = (FIXTURES / "sql/test_setup.sql").read_text()
    start = setup.index("CREATE TABLE tenk1 (")
    connection.execute(setup[start:setup.index(");", start) + 2])
    data = (FIXTURES / "data/tenk.data").read_text()
    rows = []
    for line in data.splitlines():
        cells = line.split("\t")
        assert len(cells) == 16
        rows.append(tuple(int(value) for value in cells[:13]) + tuple(cells[13:]))
    assert len(rows) == len({row[10] for row in rows}) == 10000
    with connection.cursor().copy("COPY tenk1 FROM STDIN") as copy:
        copy.write(data)
    connection.execute("ANALYZE tenk1")
    connection.execute("SET parallel_setup_cost=0; SET parallel_tuple_cost=0; SET min_parallel_table_scan_size=0; SET max_parallel_workers_per_gather=4")
    check(connection, evidence, candidate, "tenk_fixture_sort", "SELECT * FROM tenk1 ORDER BY tenthous",
          sorted(rows, key=lambda row: row[10]), required="Sort")
    evidence[-1]["tenk_data_lf_sha256"] = hashlib.sha256(data.encode()).hexdigest()
    connection.execute("RESET max_parallel_workers_per_gather")
    check(connection, evidence, candidate, "window_memory", "SELECT sum(n) OVER() FROM generate_series(1,10) a(n)", [(55,)] * 10, required="WindowAgg")
    connection.execute("SET work_mem='64kB'")
    total = 2500 * 2501 // 2
    check(connection, evidence, candidate, "window_small_budget", "SELECT sum(n) OVER() FROM generate_series(1,2500) a(n)", [(total,)] * 2500, required="WindowAgg")
    check(connection, evidence, candidate, "window_mixed_partitions",
          "SELECT sum(n) OVER(PARTITION BY m) FROM (SELECT n<3 AS m,n FROM generate_series(1,2500) a(n)) s",
          [(3,)] * 2 + [(total - 3,)] * 2498, ordered=False, required="WindowAgg")
    connection.execute("RESET work_mem")


def pruning_checks(connection, evidence, candidate):
    connection.execute("SET plan_cache_mode=force_generic_plan; SET enable_indexonlyscan=off; SET max_parallel_workers_per_gather=2")
    connection.execute("PREPARE ab_q4(int,int) AS SELECT avg(a) FROM ab WHERE a BETWEEN $1 AND $2 AND b<4")
    connection.execute("PREPARE ab_q5(int,int,int) AS SELECT avg(a) FROM ab WHERE a IN($1,$2,$3) AND b<4")
    for params in ((1, 1, 1), (2, 3, 3), (33, 44, 55)):
        check(connection, evidence, candidate, "prepared_in_" + "_".join(map(str, params)),
              "EXECUTE ab_q5(" + ",".join(map(str, params)) + ")",
              [(average([a for a, b in AB if a in params and b < 4]),)])
    check(connection, evidence, candidate, "prepared_between", "EXECUTE ab_q4(2,2)", [(average([a for a, b in AB if a == 2]),)])
    check(connection, evidence, candidate, "initplan_or", "SELECT count(*) FROM ab WHERE (a=(SELECT 1) OR a=(SELECT 3)) AND b=2",
          [(sum(a in (1, 3) and b == 2 for a, b in AB),)])
    connection.execute("CREATE TABLE lprt_a(a int NOT NULL); INSERT INTO lprt_a SELECT 0 FROM generate_series(1,100); INSERT INTO lprt_a VALUES(1),(1); ANALYZE lprt_a")
    connection.execute("SET enable_hashjoin=off; SET enable_mergejoin=off; SET enable_memoize=off")
    lookup = [0] * 100 + [1, 1]
    join = "SELECT avg(ab.a) FROM ab JOIN lprt_a x ON ab.a=x.a%s WHERE x.a IN(%s)"
    for name, expression, allowed in (("join_param", "", (0, 0, 1)), ("join_expression", "+0", (0, 0, 1)),
                                      ("join_three", "", (1, 0, 3)), ("join_one", "", (1, 0, 0)),
                                      ("join_missing", "", (1, 0, 0))):
        if name == "join_three":
            connection.execute("INSERT INTO lprt_a VALUES(3),(3)")
            lookup += [3, 3]
        if name == "join_missing":
            connection.execute("DELETE FROM lprt_a WHERE a=1")
            lookup = [a for a in lookup if a != 1]
        expected = average([a for a, _ in AB for x in lookup if a == x and x in allowed])
        check(connection, evidence, candidate, name, join % (expression, ",".join(map(str, allowed))), [(expected,)], required="Nested Loop")
    connection.execute("RESET enable_hashjoin; RESET enable_mergejoin; RESET enable_memoize")
    check(connection, evidence, candidate, "two_initplans", "SELECT * FROM ab WHERE a=(SELECT max(a) FROM lprt_a) AND b=(SELECT max(a)-1 FROM lprt_a)", [(3, 2)])
    connection.execute("CREATE TABLE listp(a int) PARTITION BY LIST(a); CREATE TABLE listp_12 PARTITION OF listp FOR VALUES IN(1,2) PARTITION BY LIST(a); CREATE TABLE listp_12_1 PARTITION OF listp_12 FOR VALUES IN(1); CREATE TABLE listp_12_2 PARTITION OF listp_12 FOR VALUES IN(2); ALTER TABLE listp_12_1 SET(parallel_workers=0); INSERT INTO listp VALUES(1),(1),(2)")
    check(connection, evidence, candidate, "nested_append", "SELECT * FROM listp WHERE a=(SELECT 1)", [(1,), (1,)], ordered=False)
    check(connection, evidence, candidate, "nested_union", "SELECT * FROM listp WHERE a=(SELECT 1) UNION ALL SELECT * FROM listp WHERE a=(SELECT 2)", [(1,), (1,), (2,)], ordered=False)
    connection.execute("CREATE TABLE ma_test(a int,b int) PARTITION BY RANGE(a)")
    for i in range(3):
        connection.execute(f"CREATE TABLE ma_test_p{i} PARTITION OF ma_test FOR VALUES FROM({i * 10}) TO({(i + 1) * 10})")
    connection.execute("INSERT INTO ma_test SELECT x,x FROM generate_series(0,29) t(x); CREATE INDEX ON ma_test(b); ANALYZE ma_test; SET enable_seqscan=off; SET enable_sort=off")
    connection.execute("PREPARE mt_q2(int) AS SELECT * FROM ma_test WHERE a >= $1 ORDER BY b LIMIT 1")
    check(connection, evidence, candidate, "merge_append_pruned", "EXECUTE mt_q2(35)", [])
    check(connection, evidence, candidate, "merge_append_nonempty", "EXECUTE mt_q2(15)", [(15, 15)])
    connection.execute("RESET enable_seqscan; RESET enable_sort")
    connection.execute("CREATE TABLE int4_tbl(f1 int); INSERT INTO int4_tbl VALUES(1),(2),(NULL),(NULL); CREATE TABLE asptab(id int PRIMARY KEY) PARTITION BY RANGE(id); CREATE TABLE asptab0 PARTITION OF asptab FOR VALUES FROM(0) TO(1); CREATE TABLE asptab1 PARTITION OF asptab FOR VALUES FROM(1) TO(2); INSERT INTO asptab VALUES(0),(1)")
    check(connection, evidence, candidate, "correlated_exists_pruning",
          "SELECT * FROM (SELECT EXISTS(SELECT 1 FROM int4_tbl tinner WHERE f1=touter.f1) AS b FROM int4_tbl touter) ss,asptab WHERE asptab.id>ss.b::int",
          [(False, 1), (False, 1)], ordered=False)


def corrupt_excluded_partitions(server):
    """Only owned, stopped cluster files are changed; restore every byte in finally."""
    with psycopg.connect(server.dsn, autocommit=True) as connection:
        locations = [connection.execute("SELECT pg_relation_filepath(%s::regclass)", (f"ab_a{a}_b{b}",)).fetchone()[0]
                     for a in range(1, 4) for b in range(1, 4) if (a, b) != (1, 2)]
    saved = []
    errors = []
    server.stop()
    try:
        for location in locations:
            path = (server.data / location).resolve()
            assert path.is_relative_to(server.data.resolve()) and path.is_file(), path
            original = path.read_bytes()
            assert 8192 <= len(original) <= 65536, (path, len(original))
            saved.append((path, original))
            path.write_bytes(b"\xff" * 8192 + original[8192:])
        server.start()
        with psycopg.connect(server.dsn, autocommit=True) as connection:
            connection.execute("SET plan_cache_mode=force_generic_plan; SET max_parallel_workers_per_gather=0; SET enable_indexonlyscan=off; SET enable_indexscan=off; SET enable_bitmapscan=off")
            connection.execute("PREPARE fault_prune(int,int) AS SELECT count(*) FROM ab WHERE a=$1 AND b=$2")
            assert connection.execute("EXECUTE fault_prune(1,2)").fetchall() == [(2,)]
            for a, b in ((2, 2), (1, 1)):
                try:
                    connection.execute(f"EXECUTE fault_prune({a},{b})").fetchall()
                except psycopg.Error as error:
                    assert error.sqlstate == "XX001", (a, b, error.sqlstate, str(error))
                    errors.append({"a": a, "b": b, "sqlstate": error.sqlstate})
                else:
                    raise AssertionError("selected corrupted partition did not fail")
            assert connection.execute("EXECUTE fault_prune(1,2)").fetchall() == [(2,)]
    finally:
        try:
            server.stop()
        finally:
            for path, original in saved:
                path.write_bytes(original)
                assert path.read_bytes() == original, ("incomplete restoration", path)
    server.start()
    with psycopg.connect(server.dsn, autocommit=True) as connection:
        restored = connection.execute("SELECT a,b FROM ab ORDER BY a,b").fetchall()
        assert restored == sorted(AB), restored
    return {"excluded_corrupted_partitions": len(saved), "selected_partition_rows": 2,
            "negative_controls": errors, "files_restored_exactly": True,
            "restored_rows": len(restored), "restored_values_sha256": digest(restored)}


def update_checks(connection, evidence, candidate):
    for name, initial, sql, expected in (
        ("update_partition_subtree", [(1, 2)], "UPDATE ab_a1 SET b=3 FROM ab WHERE ab.a=1 AND ab.a=ab_a1.a", [("ab_a1_b3", 1, 3)]),
        ("update_pruned_source", [(1, 1), (1, 2), (1, 3), (2, 1)], "UPDATE ab_a1 SET b=3 FROM ab_a2 WHERE ab_a2.b=(SELECT 1)",
         [("ab_a1_b3", 1, 3)] * 3 + [("ab_a2_b1", 2, 1)]),
    ):
        connection.execute("TRUNCATE ab")
        with connection.cursor() as cursor:
            cursor.executemany("INSERT INTO ab VALUES(%s,%s)", initial)
        document = connection.execute("EXPLAIN (ANALYZE,FORMAT JSON,TIMING OFF,BUFFERS OFF) " + sql).fetchone()[0][0]
        assert document["Plan"]["Actual Rows"] == 0 and document["Plan"]["Actual Loops"] == 1
        if candidate:
            for node in nodes(document["Plan"]):
                assert not UNMEASURED.get(node["Node Type"], set()) & node.keys()
        cursor = connection.execute("SELECT tableoid::regclass::text,a,b FROM ab ORDER BY 1,2,3")
        rows = cursor.fetchall()
        assert rows == expected, (name, expected, rows)
        evidence.append({"name": name, "sql": sql, "types": [d.type_code for d in cursor.description],
                         "rows": len(rows), "values_sha256": digest(rows), "explain": document})


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    for name in ("reference", "candidate", "work-dir", "output"):
        ap.add_argument("--" + name, type=Path, required=True)
    args = ap.parse_args()
    report = {"passed": False, "engines": {}}
    try:
        for name, binaries in (("native", args.reference), ("candidate", args.candidate)):
            with Cluster(binaries, args.work_dir, reference=name == "native", settings=["autovacuum=off"]) as server:
                with (binaries / "postgres").open("rb") as file:
                    identity = hashlib.file_digest(file, "sha256").hexdigest()
                engine = {"cluster": str(server.root), "binary_sha256": identity, "version": server.version, "checks": []}
                report["engines"][name] = engine
                with psycopg.connect(server.dsn, autocommit=True) as connection:
                    setup_partitions(connection)
                    explain_checks(connection, engine["checks"], name == "candidate")
                    pruning_checks(connection, engine["checks"], name == "candidate")
                engine["corruption_pruning"] = corrupt_excluded_partitions(server)
                with psycopg.connect(server.dsn, autocommit=True) as connection:
                    update_checks(connection, engine["checks"], name == "candidate")
        native, candidate = (report["engines"][name] for name in ("native", "candidate"))
        assert native["corruption_pruning"] == candidate["corruption_pruning"]
        assert len(native["checks"]) == len(candidate["checks"])
        for p, c in zip(native["checks"], candidate["checks"]):
            assert all(p[key] == c[key] for key in ("name", "types", "rows", "values_sha256")), (p["name"], p, c)
        report["passed"] = True
    except Exception as error:
        report["error"] = str(error)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2, default=str) + "\n")
    print(json.dumps({key: value for key, value in report.items() if key != "engines"}))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())

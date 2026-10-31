#!/usr/bin/env python3
"""Check SQL results behind diagnostic fixtures and real root row counters."""
import argparse
import hashlib
import json
from pathlib import Path

import psycopg
from cluster import Cluster
from explain_contract import nodes, UNMEASURED


SETUP = """
CREATE TABLE diag_numeric(id int, x numeric, t text);
INSERT INTO diag_numeric
 SELECT i,x,x::text FROM (SELECT i,round((i/pi())::numeric,7) x FROM generate_series(1,20) i) s;
INSERT INTO diag_numeric SELECT id+20,x,t FROM diag_numeric;
CREATE INDEX diag_numeric_idx ON diag_numeric(x,t);
CREATE TABLE diag_float(id int, f double precision);
INSERT INTO diag_float VALUES (1,'-0.0'),(2,'+0.0');
CREATE INDEX diag_float_idx ON diag_float(f);
CREATE TABLE diag_text(id int, n name, t text);
INSERT INTO diag_text VALUES (1,'one','one'),(2,'two','two'),(3,'three',repeat(md5('three'),100));
INSERT INTO diag_text SELECT id+3,n,t FROM diag_text;
CREATE INDEX diag_name_idx ON diag_text(n);
CREATE INDEX diag_text_idx ON diag_text(t);
CREATE TABLE diag_limit(pk int PRIMARY KEY,c1 int,c2 int);
INSERT INTO diag_limit SELECT i,(i-1)%4+1,(i-1)%4+1 FROM generate_series(1,8) i;
CREATE TABLE diag_incremental(a int,b int);
INSERT INTO diag_incremental SELECT i/100+1,i+1 FROM generate_series(0,999) i;
ANALYZE;
SET enable_hashjoin=off;
SET enable_mergejoin=off;
"""
QUERIES = {
    "numeric_expression_keys": "SELECT a.id,b.id FROM diag_numeric a JOIN diag_numeric b ON a.x=b.t::numeric AND a.t::numeric=b.x ORDER BY 1,2",
    "float_logical_keys": "SELECT a.id,b.id FROM diag_float a JOIN diag_float b ON a.f=b.f ORDER BY 1,2",
    "float_binary_keys": "SELECT a.id,b.id FROM diag_float a JOIN diag_float b ON a.f>=b.f ORDER BY 1,2",
    "name_binary_keys": "SELECT a.id,b.id FROM diag_text a JOIN diag_text b ON a.n>=b.n ORDER BY 1,2",
    "text_binary_keys": "SELECT a.id,b.id FROM diag_text a JOIN diag_text b ON a.t>=b.t ORDER BY 1,2",
    "projected_subquery_topk": "SELECT * FROM (SELECT pk,c2 FROM diag_limit ORDER BY c1,pk) x LIMIT 3",
    "incremental_prefix_55": "SELECT * FROM (SELECT * FROM diag_incremental ORDER BY a) s ORDER BY a,b LIMIT 55",
    "incremental_prefix_70": "SELECT * FROM (SELECT * FROM diag_incremental ORDER BY a) s ORDER BY a,b LIMIT 70",
}
EXPECTED_COUNTS = [80, 4, 4, 24, 24, 3, 55, 70]


def exercise(connection, candidate):
    connection.execute(SETUP, prepare=False)
    inputs = {}
    for name, query in {
        "numeric": "SELECT id,x::text,t FROM diag_numeric ORDER BY id",
        "float_bits": "SELECT id,encode(float8send(f),'hex') FROM diag_float ORDER BY id",
        "strings": "SELECT id,n::text,t FROM diag_text ORDER BY id",
        "limit": "SELECT * FROM diag_limit ORDER BY pk",
        "incremental": "SELECT * FROM diag_incremental ORDER BY b",
    }.items():
        cursor = connection.execute(query)
        rows = cursor.fetchall()
        inputs[name] = {"types": [column.type_code for column in cursor.description], "rows": len(rows),
                        "sha256": hashlib.sha256(json.dumps(rows).encode()).hexdigest()}
    results = []
    for (name, query), count in zip(QUERIES.items(), EXPECTED_COUNTS):
        force_index = name in {"float_logical_keys", "float_binary_keys", "name_binary_keys", "text_binary_keys"}
        connection.execute("SET enable_seqscan=" + ("off" if force_index else "on"))
        cursor = connection.execute(query, prepare=False)
        types = [column.type_code for column in cursor.description]
        rows = cursor.fetchall()
        assert len(rows) == count, (name, len(rows), count)
        document = connection.execute("EXPLAIN (ANALYZE,FORMAT JSON,TIMING OFF,BUFFERS OFF) " + query).fetchone()[0][0]
        plan = document["Plan"]
        kinds = {node["Node Type"] for node in nodes(plan)}
        if name.endswith("_keys"):
            assert "Memoize" in kinds, (name, "required Memoize path missing", kinds)
        if name.startswith("incremental_prefix"):
            assert "Incremental Sort" in kinds, (name, kinds)
        assert plan["Actual Rows"] == count and plan["Actual Loops"] == 1, (name, plan)
        if candidate:
            for node in nodes(plan):
                assert not (UNMEASURED.get(node["Node Type"], set()) & node.keys()), (name, node)
        results.append({"name": name, "types": types, "rows": rows, "explain": document})
    connection.execute("SET enable_seqscan=on")
    # Keep the sorted cursor alive while observing its actual allocator inventory.
    with connection.transaction():
        connection.execute("DECLARE diag_cur CURSOR FOR SELECT left(a,10),b FROM (VALUES(repeat('a',512*1024),1),(repeat('b',512),2)) v(a,b) ORDER BY a DESC")
        assert connection.execute("FETCH 1 FROM diag_cur").fetchall() == [("bbbbbbbbbb", 2)]
        inventory = connection.execute("SELECT name,type,level,total_bytes,free_bytes,used_bytes FROM pg_backend_memory_contexts").fetchall()
        for name, kind, level, total, free, used in inventory:
            assert level >= 1 and total >= 0 and free >= 0 and used >= 0 and total == free + used, (name, kind, total, free, used)
        assert any(row[0] == "TopMemoryContext" and row[2] == 1 for row in inventory), inventory
        caller = [row for row in inventory if row[0] == "Caller tuples"]
        assert bool(caller) != candidate, ("unexpected caller allocator", caller)
        assert connection.execute("FETCH ALL FROM diag_cur").fetchall() == [("aaaaaaaaaa", 1)]
        results.append({"name": "cursor_allocator_inventory", "caller_contexts": caller,
                        "context_count": len(inventory), "all_byte_totals_consistent": True})
    connection.execute("CREATE ROLE diag_reader")
    connection.execute("SET ROLE diag_reader")
    try:
        connection.execute("SELECT * FROM pg_backend_memory_contexts").fetchall()
    except psycopg.Error as exc:
        assert exc.sqlstate == "42501", exc
    else:
        raise AssertionError("unprivileged role could inspect backend memory contexts")
    finally:
        connection.execute("RESET ROLE")
    connection.execute("GRANT pg_read_all_stats TO diag_reader")
    connection.execute("SET ROLE diag_reader")
    assert connection.execute("SELECT count(*) FROM pg_backend_memory_contexts").fetchone()[0] > 0
    connection.execute("RESET ROLE")
    results.append({"name": "memory_context_view_role_boundary", "passed": True})
    return {"checks": results, "inputs": inputs}


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--reference", type=Path, required=True)
    ap.add_argument("--candidate", type=Path, required=True)
    ap.add_argument("--work-dir", type=Path, required=True)
    ap.add_argument("--output", type=Path, required=True)
    args = ap.parse_args()
    report = {"passed": False, "engines": {}}
    try:
        for name, binaries in (("native", args.reference), ("candidate", args.candidate)):
            with Cluster(binaries, args.work_dir, reference=name == "native", settings=["max_parallel_workers_per_gather=0"]) as server:
                with (binaries / "postgres").open("rb") as binary:
                    identity = hashlib.file_digest(binary, "sha256").hexdigest()
                engine = {"cluster": str(server.root), "version": server.version, "binary_sha256": identity}
                report["engines"][name] = engine
                with psycopg.connect(server.dsn, autocommit=True) as conn:
                    engine.update(exercise(conn, name == "candidate"))
        assert report["engines"]["native"]["inputs"] == report["engines"]["candidate"]["inputs"], "input datasets differ"
        for p, c in zip(report["engines"]["native"]["checks"][:8], report["engines"]["candidate"]["checks"][:8]):
            assert (p["name"], p["types"], p["rows"]) == (c["name"], c["types"], c["rows"]), (p["name"], p, c)
        report["passed"] = True
    except Exception as exc:
        report["error"] = str(exc)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({k: v for k, v in report.items() if k != "engines"}, indent=2))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())

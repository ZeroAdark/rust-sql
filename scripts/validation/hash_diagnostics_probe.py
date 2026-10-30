#!/usr/bin/env python3
"""Replay the self-contained hash diagnostic transaction with typed result checks."""
import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path

import psycopg
from cluster import Cluster
from compare_regression import ORDER, columns, find_anchor, physical_lines, statements, table
from diagnostic_policy import bindings, digest
from explain_contract import nodes, UNMEASURED


FIXTURES = Path(__file__).resolve().parents[2] / "crates/postgres-18.6-reference/src/test/regress"


def fixture_transaction():
    source = (FIXTURES / "sql/join_hash.sql").read_text()
    expected = physical_lines((FIXTURES / "expected/join_hash.out").read_text())
    rules = bindings(source)
    assert rules, "fixture does not match reviewed diagnostic policy"
    selected, position = [], 0
    for line, anchor, sql in statements(source):
        start = find_anchor(expected, anchor, position)
        assert start is not None, line
        position = start + len(anchor)
        tokens = ORDER.lex(sql)
        rule = rules.get(digest(sql))
        values, ordered = None, True
        if tokens[0] == ("w", "select") and rule is None:
            end, rows = table(expected, position)
            labels, spans = columns(expected[position], expected[position + 1])
            # This transaction's data results are only integer counts/lengths
            # and nullable int4 IDs. Unknown shapes must be reviewed again.
            values = [tuple(int(row[a:b].strip()) if row[a:b].strip() else None for a, b in spans) for row in rows]
            ordered = ORDER.classify(sql, labels)[0] != "unordered"
        selected.append((line, sql, tokens[0][1], rule, values, ordered))
        if sql.lower() == "rollback;":
            break
    assert sum(item[3] is not None for item in selected) == 18
    return source, selected


def execute_case(connection, evidence, candidate, statement):
    line, sql, kind, rule, expected, ordered = statement
    cursor = connection.execute(sql, prepare=False)
    if kind == "explain" or cursor.description is None:
        if cursor.description:
            cursor.fetchall()
        return
    values = cursor.fetchall()
    types = [column.type_code for column in cursor.description]
    record = {"sql_line": line, "sql_sha256": digest(sql), "types": types, "values": values}
    if rule is None:
        assert expected is not None and all(oid in (20, 23) for oid in types), (line, types)
        assert (values == expected if ordered else Counter(values) == Counter(expected)), (line, expected, values)
        record["kind"] = "query_data"
        record["ordered"] = ordered
    else:
        assert rule["kind"] == "unavailable_hash_metrics" and len(values) == 1
        if candidate:
            assert values == [tuple(None for _ in types)], (line, values)
        assert sql.count("$$") == 2
        inner = sql.split("$$")[1].strip().removesuffix(";")
        assert ORDER.lex(inner)[0] == ("w", "select")
        inner_cursor = connection.execute(inner, prepare=False)
        inner_values = inner_cursor.fetchall()
        assert len(inner_values) == 1 and len(inner_values[0]) == 1
        document = connection.execute("EXPLAIN (ANALYZE,FORMAT JSON,TIMING OFF,BUFFERS OFF) " + inner).fetchone()[0][0]
        root = document["Plan"]
        assert root["Actual Rows"] == 1 and root["Actual Loops"] == 1, (line, root)
        if candidate:
            for node in nodes(root):
                assert not UNMEASURED.get(node["Node Type"], set()) & node.keys(), (line, node)
        record.update(kind="unavailable_metric", inner_types=[d.type_code for d in inner_cursor.description],
                      inner_values=inner_values, explain=document)
    evidence.append(record)


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    for name in ("reference", "candidate", "work-dir", "output"):
        ap.add_argument("--" + name, type=Path, required=True)
    args = ap.parse_args()
    report = {"passed": False, "engines": {}}
    try:
        source, selected = fixture_transaction()
        report.update(source_sha256=digest(source), statements=len(selected))
        for name, binaries in (("native", args.reference), ("candidate", args.candidate)):
            with Cluster(binaries, args.work_dir, reference=name == "native", settings=["autovacuum=off"]) as server:
                with (binaries / "postgres").open("rb") as file:
                    identity = hashlib.file_digest(file, "sha256").hexdigest()
                engine = {"cluster": str(server.root), "binary_sha256": identity, "version": server.version, "checks": []}
                report["engines"][name] = engine
                with psycopg.connect(server.dsn, autocommit=True) as connection:
                    connection.execute("SET statement_timeout='60s'")
                    for statement in selected:
                        execute_case(connection, engine["checks"], name == "candidate", statement)
        native, candidate = (report["engines"][name]["checks"] for name in ("native", "candidate"))
        assert len(native) == len(candidate)
        for p, c in zip(native, candidate):
            assert all(p[key] == c[key] for key in ("sql_line", "sql_sha256", "kind", "types"))
            if p["kind"] == "unavailable_metric":
                assert (p["inner_types"], p["inner_values"]) == (c["inner_types"], c["inner_values"]), (p, c)
            else:
                assert (p["values"] == c["values"] if p["ordered"] else Counter(p["values"]) == Counter(c["values"])), (p, c)
        assert any(record["kind"] == "unavailable_metric" and all(value is not None for value in record["values"][0]) for record in native)
        report["passed"] = True
    except Exception as error:
        report["error"] = str(error)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({key: value for key, value in report.items() if key != "engines"}))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())

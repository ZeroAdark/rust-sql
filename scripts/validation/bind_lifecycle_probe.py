#!/usr/bin/env python3
"""Check parameter ownership across protocol errors, portal reuse, and rollback.

Run against both native PostgreSQL and the candidate. This is correctness
validation, not a performance measurement.
"""
import argparse
from decimal import Decimal
import hashlib
import json
from pathlib import Path

import psycopg
from psycopg.pq import DiagnosticField, ExecStatus

from connection_churn_probe import CheckedCluster


def exercise(cluster):
    checks = []
    with psycopg.connect(cluster.dsn, autocommit=True) as admin:
        admin.execute("CREATE ROLE bind_user LOGIN")
        admin.execute("CREATE TABLE bind_probe(id integer PRIMARY KEY, v bigint, label text)")
        admin.execute("INSERT INTO bind_probe SELECT i,0,'row'||i FROM generate_series(0,63) s(i)")
        admin.execute("GRANT SELECT,UPDATE ON bind_probe TO bind_user")
        with psycopg.connect(cluster.dsn, user="bind_user", autocommit=True) as conn:
            conn.execute("SET statement_timeout='10s'")
            conn.execute("SET log_parameter_max_length_on_error=64")
            balances = [0] * 64
            for mode in ("auto", "force_generic_plan", "force_custom_plan"):
                conn.execute(f"SET plan_cache_mode={mode}")
                for i in range(120):
                    key = (i * 17) % 64
                    row = conn.execute("SELECT id,v FROM bind_probe WHERE id=%s", (key,), prepare=True).fetchone()
                    assert row == (key, balances[key]), (mode, key, row)
                for key in range(64):
                    balances[key] += 1
                    row = conn.execute("UPDATE bind_probe SET v=v+1 WHERE id=%s RETURNING v", (key,), prepare=True).fetchone()
                    assert row == (balances[key],)
                checks.append("point_read_and_update_" + mode)

            # By-reference input values still need the context lifetime, even
            # when the parameter array itself can now be freed individually.
            for i in range(40):
                values = (
                    None if i % 3 == 0 else 2**40 + i,
                    None if i % 5 == 0 else "caf\u00e9-" * (1 if i % 2 else 8192),
                    None if i % 7 == 0 else bytes(range(256)) * (1 if i % 2 else 128),
                    [i, None, -i],
                    Decimal("12345678901234567890.0123456789") + i,
                )
                row = conn.execute(
                    "SELECT %s::bigint,%s::text,%s::bytea,%s::integer[],%s::numeric",
                    values, prepare=True,
                ).fetchone()
                assert row == values, (i, "by-reference or null parameter changed")
            checks.append("mixed_value_sizes_nulls_arrays_and_numeric")

            # The invalid result format fails after Bind has registered its
            # parameter array; malformed input formats fail before registration.
            result = conn.pgconn.prepare(b"bind_wire", b"SELECT $1::integer")
            assert result.status == ExecStatus.COMMAND_OK, result.error_message
            errors = {}
            for name, value, input_format, result_format in (
                ("result_format_after_registration", b"7", 0, 2),
                ("input_format_before_registration", b"7", 2, 0),
                ("short_binary_input", b"\x00", 1, 0),
            ):
                for i in range(12):
                    result = conn.pgconn.exec_prepared(
                        b"bind_wire", [value], param_formats=[input_format], result_format=result_format,
                    )
                    assert result.status == ExecStatus.FATAL_ERROR, (name, result.status)
                    state = result.error_field(DiagnosticField.SQLSTATE).decode()
                    errors.setdefault(name, state)
                    assert errors[name] == state
                    assert conn.execute("SELECT %s::integer", (i,), prepare=True).fetchone() == (i,)
            assert errors["result_format_after_registration"] == "22023"
            assert errors["input_format_before_registration"] == "22023"
            checks.append("bind_error_cleanup_and_subsequent_reuse")

            for i in range(12):
                try:
                    with conn.transaction():
                        conn.execute("SELECT 10 / %s::integer", (0,), prepare=True)
                except psycopg.errors.DivisionByZero:
                    pass
                else:
                    raise AssertionError("division did not fail")
                assert conn.execute("SELECT %s::text", ("after rollback",), prepare=True).fetchone() == ("after rollback",)
            checks.append("execute_error_rollback_and_reuse")

            with conn.transaction():
                cursor = conn.cursor(name="held_bind", withhold=True)
                cursor.execute("SELECT id,label FROM bind_probe WHERE id>=%s ORDER BY id", (55,))
                first = cursor.fetchmany(3)
            rest = cursor.fetchall()
            cursor.close()
            assert first + rest == [(i, "row" + str(i)) for i in range(55, 64)]
            checks.append("named_holdable_cursor_survives_commit")

            admin.execute("ALTER TABLE bind_probe ADD COLUMN extra integer")
            for key in range(64):
                assert conn.execute("SELECT id,v FROM bind_probe WHERE id=%s", (key,), prepare=True).fetchone() == (key, 3)
            checks.append("prepared_plan_revalidation")
        assert admin.execute("SELECT sum(v) FROM bind_probe").fetchone() == (192,)
    return {"checks": checks, "wire_sqlstates": errors, "committed_updates": 192}


def run(args):
    report = {"passed": False, "engines": {}}
    try:
        for label, binaries, reference in (("postgresql", args.reference, True), ("rustsql", args.candidate, False)):
            cluster = CheckedCluster(binaries, args.work_dir, reference=reference)
            entry = {"cluster": str(cluster.root), "binary_sha256": hashlib.sha256((binaries / "postgres").read_bytes()).hexdigest()}
            report["engines"][label] = entry
            with cluster:
                entry["version"] = cluster.version
                entry["result"] = exercise(cluster)
            entry["shutdown_exit_codes"] = cluster.shutdown_exit_codes
        assert report["engines"]["postgresql"]["result"] == report["engines"]["rustsql"]["result"]
        report["passed"] = True
    except Exception as error:
        report["failure"] = f"{type(error).__name__}: {error}"
    return report


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    for name in ("candidate", "reference", "work-dir", "output"):
        ap.add_argument("--" + name, type=Path, required=True)
    args = ap.parse_args()
    report = run(args)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())

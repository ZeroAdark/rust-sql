#!/usr/bin/env python3
"""Crash/restart, logical restore and SQL role-boundary checks on owned clusters."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess

import psycopg
from psycopg.conninfo import make_conninfo
from cluster import Cluster


def cgroup_snapshot():
    for line in Path("/proc/self/cgroup").read_text().splitlines():
        if line.startswith("0::"):
            directory = Path("/sys/fs/cgroup") / line[3:].lstrip("/")
            return {name: (directory / name).read_text().strip() for name in
                    ("memory.max", "memory.peak", "memory.events", "memory.swap.max", "cpu.max", "cpu.stat")
                    if (directory / name).exists()}
    return {}


def digest(conn, table):
    # Table names are the fixed fixtures below, never caller-supplied SQL.
    rows = conn.execute(f"SELECT id,n,payload FROM {table} ORDER BY id").fetchall()
    return {"rows": len(rows), "sha256": hashlib.sha256(json.dumps(rows, separators=(",", ":")).encode()).hexdigest()}


def run(args, report):
    with Cluster(args.candidate, args.work_dir) as source:
        report["source"] = str(source.root)
        with psycopg.connect(source.dsn, autocommit=True) as conn:
            assert conn.execute("SELECT current_setting('fsync'),current_setting('synchronous_commit')").fetchone() == ("on", "on")
            conn.execute("CREATE ACCESS METHOD pgrcolumnar TYPE TABLE HANDLER heap_tableam_handler; "
                         "CREATE TABLE heap_ledger(id integer PRIMARY KEY,n integer NOT NULL,payload text); "
                         "CREATE TABLE col_ledger(id integer,n integer NOT NULL,payload text) USING pgrcolumnar", prepare=False)
        expected = {}
        for cycle in range(args.cycles):
            with psycopg.connect(source.dsn, autocommit=True) as conn:
                with conn.transaction():
                    for table in ("heap_ledger", "col_ledger"):
                        start, end = cycle * 100 + 1, (cycle + 1) * 100
                        conn.execute(f"INSERT INTO {table} SELECT i,i,repeat('payload-'||i,8) FROM generate_series(%s::integer,%s::integer) g(i)", (start, end))
                        conn.execute(f"UPDATE {table} SET n=n+1 WHERE id%7=0")
                        conn.execute(f"DELETE FROM {table} WHERE id%31=0")
                if cycle % 2 == 0:
                    conn.execute("CHECKPOINT")
                expected = {table: digest(conn, table) for table in ("heap_ledger", "col_ledger")}
            pending = psycopg.connect(source.dsn)
            try:
                pending.execute("UPDATE heap_ledger SET n=-100; UPDATE col_ledger SET n=-100; "
                                "CREATE TABLE uncommitted_ddl AS SELECT 1 AS marker", prepare=False)
                source.stop(crash=True)  # Kills supervisor and server child in their private process group.
            finally:
                pending.close()
            source.start()
            with psycopg.connect(source.dsn, autocommit=True) as conn:
                got = {table: digest(conn, table) for table in expected}
                if got != expected or conn.execute("SELECT to_regclass('uncommitted_ddl')").fetchone()[0] is not None:
                    raise AssertionError(f"crash cycle {cycle}: committed data or transactional DDL mismatch")
            report["checks"].append(f"whole_process_crash_{cycle + 1}")

        # Role tests exercise authorization, not authentication: these clusters use private trust sockets.
        with psycopg.connect(source.dsn, autocommit=True) as conn:
            conn.execute("CREATE ROLE probe_reader LOGIN; GRANT SELECT ON heap_ledger TO probe_reader", prepare=False)
        with psycopg.connect(make_conninfo(source.dsn, user="probe_reader"), autocommit=True) as reader:
            assert reader.execute("SELECT count(*) FROM heap_ledger").fetchone()[0] == expected["heap_ledger"]["rows"]
            for statement in ("INSERT INTO heap_ledger VALUES(0,0,'forbidden')", "ALTER ROLE probe_reader SUPERUSER",
                              "SET ROLE rustsql", "SELECT * FROM col_ledger"):
                try:
                    reader.execute(statement)
                except psycopg.Error as error:
                    if error.sqlstate != "42501":
                        raise
                else:
                    raise AssertionError(f"role boundary failed: {statement}")
            reader.execute("SET statement_timeout='40ms'")
            try:
                reader.execute("SELECT pg_sleep(1)")
            except psycopg.Error as error:
                if error.sqlstate != "57014":
                    raise
            else:
                raise AssertionError("statement timeout did not cancel sleep")
            assert reader.execute("SELECT 1").fetchone() == (1,)
        report["checks"].append("ordinary_role_and_timeout_recovery")

        dump = source.root / "logical-backup.dump"
        command = [str(args.reference_clients / "pg_dump"), "--format=custom", f"--file={dump}"]
        result = subprocess.run(command, env=source.env, capture_output=True, text=True, timeout=120)
        (source.root / "dump.log").write_text(result.stdout + result.stderr)
        result.check_returncode()
        with Cluster(args.candidate, args.work_dir) as restored:
            report["restored"] = str(restored.root)
            # pg_dump preserves ACLs but not roles, which are cluster-wide objects.
            with psycopg.connect(restored.dsn, autocommit=True) as conn:
                conn.execute("CREATE ROLE probe_reader LOGIN")
            command = [str(args.reference_clients / "pg_restore"), "--exit-on-error", "--single-transaction",
                       "--dbname=postgres", str(dump)]
            result = subprocess.run(command, env=restored.env, capture_output=True, text=True, timeout=120)
            (restored.root / "restore.log").write_text(result.stdout + result.stderr)
            result.check_returncode()
            with psycopg.connect(restored.dsn, autocommit=True) as conn:
                got = {table: digest(conn, table) for table in expected}
                if got != expected:
                    raise AssertionError("restored logical data differs")
                methods = conn.execute("SELECT c.relname,a.amname FROM pg_class c JOIN pg_am a ON c.relam=a.oid WHERE c.relname IN ('heap_ledger','col_ledger') ORDER BY 1").fetchall()
                assert methods == [("col_ledger", "pgrcolumnar"), ("heap_ledger", "heap")]
                assert conn.execute("SELECT has_table_privilege('probe_reader','heap_ledger','SELECT'),has_table_privilege('probe_reader','col_ledger','SELECT')").fetchone() == (True, False)
            report["checks"].append("logical_restore_rows_storage_methods_and_acl")
        report["data"] = expected
        report["backup_sha256"] = hashlib.sha256(dump.read_bytes()).hexdigest()
    report["passed"] = True


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--candidate", type=Path, required=True)
    ap.add_argument("--reference-clients", type=Path, required=True)
    ap.add_argument("--work-dir", type=Path, required=True)
    ap.add_argument("--output", type=Path, required=True)
    ap.add_argument("--cycles", type=int, default=10)
    args = ap.parse_args()
    if args.cycles < 1:
        ap.error("at least one crash cycle required")
    report = {"passed": False, "checks": [], "cgroup_before": cgroup_snapshot()}
    try:
        run(args, report)
    except Exception as error:
        report["error"] = f"{type(error).__name__}: {error}"
    finally:
        report["cgroup_after"] = cgroup_snapshot()
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())

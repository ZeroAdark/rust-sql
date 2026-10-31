#!/usr/bin/env python3
"""Reproduce operator-family invalidation racing concurrent index deletion.

Every object belongs to a fresh private cluster. SQL errors are retained, never
retried or counted as successful operations. Run a native PostgreSQL control too.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import json
from pathlib import Path
import threading
import psycopg
from cluster import Cluster


def exercise(dsn, iterations, workers):
    with psycopg.connect(dsn, autocommit=True) as connection:
        connection.execute("""CREATE TYPE churn_int;
          CREATE FUNCTION churn_int_in(cstring) RETURNS churn_int LANGUAGE internal IMMUTABLE STRICT AS 'int8in';
          CREATE FUNCTION churn_int_out(churn_int) RETURNS cstring LANGUAGE internal IMMUTABLE STRICT AS 'int8out';
          CREATE TYPE churn_int(INPUT=churn_int_in,OUTPUT=churn_int_out,LIKE=int8);
          CREATE FUNCTION churn_int_eq(churn_int,churn_int) RETURNS bool LANGUAGE internal IMMUTABLE STRICT AS 'int8eq';
          CREATE OPERATOR =(PROCEDURE=churn_int_eq,LEFTARG=churn_int,RIGHTARG=churn_int,MERGES)""")
        # Keep enough family members that deletion can intersect the candidate's
        # pg_index scan followed by pg_class lookup, without artificial sleeps.
        for i in range(workers):
            connection.execute(f"CREATE TABLE churn_{i}(v bigint)")
        connection.execute("CREATE TABLE stable(v bigint)")
        for i in range(96):
            connection.execute(f"CREATE INDEX stable_{i} ON stable(v)")
    barrier = threading.Barrier(workers + 1)
    stop = threading.Event()
    def run(worker):
        completed = 0
        errors = []
        try:
            with psycopg.connect(dsn, autocommit=True) as connection:
                connection.execute("SET statement_timeout='15s'")
                barrier.wait(timeout=30)
                for i in range(iterations):
                    if stop.is_set():
                        break
                    statements = (
                        ["ALTER OPERATOR FAMILY integer_ops USING btree ADD OPERATOR 3 =(churn_int,churn_int)",
                         "ALTER OPERATOR FAMILY integer_ops USING btree DROP OPERATOR 3 (churn_int,churn_int)"]
                        if worker == -1 else
                        [f"CREATE INDEX churn_index_{worker} ON churn_{worker}(v)", f"DROP INDEX churn_index_{worker}"])
                    for sql in statements:
                        try:
                            connection.execute(sql)
                            completed += 1
                        except psycopg.Error as error:
                            errors.append({"iteration": i, "sql": sql, "sqlstate": error.sqlstate,
                                           "message": error.diag.message_primary})
                            stop.set()
                            break
        except Exception as error:
            errors.append({"error": str(error)})
            stop.set()
            barrier.abort()
        return {"worker": worker, "completed_statements": completed, "errors": errors}
    with ThreadPoolExecutor(max_workers=workers + 1) as executor:
        futures = [executor.submit(run, worker) for worker in range(-1, workers)]
        return [future.result() for future in futures]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("reference", "candidate", "work-dir", "output"):
        parser.add_argument("--" + name, required=True, type=Path)
    parser.add_argument("--iterations", type=int, default=200)
    parser.add_argument("--workers", type=int, default=3)
    args = parser.parse_args()
    if args.iterations < 1 or not 1 <= args.workers <= 16:
        parser.error("positive iterations and 1..16 workers required")
    report = {"passed": False, "iterations": args.iterations, "workers": args.workers, "servers": {}}
    try:
        for name, binaries, reference in (("postgresql", args.reference, True), ("candidate", args.candidate, False)):
            with Cluster(binaries, args.work_dir, reference=reference) as server:
                report["servers"][name] = {"version": server.version, "cluster": str(server.root),
                                             "workers": exercise(server.dsn, args.iterations, args.workers)}
        report["passed"] = all(not worker["errors"] and worker["completed_statements"] == args.iterations * 2
                               for server in report["servers"].values() for worker in server["workers"])
    except Exception as error:
        report["error"] = str(error)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())

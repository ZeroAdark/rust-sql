#!/usr/bin/env python3
"""Exercise a freshly built server in an owned, disposable local cluster.

Requires Linux, initdb/postgres binaries and psql. Never connects to an existing
cluster. Keeps the temporary directory and logs for diagnosis, even on success.
"""
import argparse
import concurrent.futures
import json
import os
from pathlib import Path
import signal
import shutil
import subprocess
import tempfile
import time


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin-dir", type=Path, required=True)
    ap.add_argument("--psql", default="psql")
    ap.add_argument("--output", type=Path, required=True)
    ap.add_argument("--work-dir", type=Path, help="parent for retained disposable clusters (default: system temp)")
    ap.add_argument("--mixed-seconds", type=int, default=0, help="optional mixed probe; requires psycopg 3")
    ap.add_argument("--regression-dir", type=Path, help="built PostgreSQL 18.6 src/test/regress directory")
    ap.add_argument("--isolation-dir", type=Path, help="built PostgreSQL 18.6 src/test/isolation directory")
    ap.add_argument("--isolation-overlay", type=Path, help="copy documented engine isolation fixtures into an owned input tree")
    ap.add_argument("--reference-bin", type=Path, help="PostgreSQL 18.6 client directory for pg_regress")
    args = ap.parse_args()
    if os.geteuid() == 0:
        ap.error("run as a non-root user")
    bins = args.bin_dir.resolve()
    if args.work_dir is not None:
        args.work_dir.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="rust-sql-smoke-", dir=args.work_dir))
    data, sock = root / "data", root / "socket"
    sock.mkdir(mode=0o700)
    env = {k: v for k, v in os.environ.items() if not k.startswith("PG")}
    env.update(PGHOST=str(sock), PGPORT="55439", PGUSER="rustsql", PGDATABASE="postgres",
               PGCONNECT_TIMEOUT="2", RUST_MIN_STACK="33554432")
    report = {"cluster": str(root), "checks": [], "passed": False}
    server = None
    log = None

    def sql(statement, timeout=60):
        p = subprocess.run([args.psql, "-X", "-qAt", "-v", "ON_ERROR_STOP=1", "-c", statement],
                           env=env, text=True, capture_output=True, timeout=timeout)
        if p.returncode:
            raise RuntimeError(f"SQL failed: {statement}\n{p.stderr}")
        return p.stdout.strip()

    def check(name, statement, expected):
        got = sql(statement)
        if got != expected:
            raise AssertionError(f"{name}: expected {expected!r}, got {got!r}")
        report["checks"].append(name)

    def start():
        nonlocal server, log
        log = (root / "server.log").open("a")
        server = subprocess.Popen([str(bins / "postgres"), "-D", str(data), "-k", str(sock),
                                   "-p", "55439", "-c", "listen_addresses=", "-c", "io_method=sync",
                                   "-c", "max_stack_depth=60000"], env=env, stdout=log, stderr=log)
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            if server.poll() is not None:
                raise RuntimeError(f"server exited: see {root / 'server.log'}")
            try:
                if sql("SELECT 1", timeout=3) == "1":
                    return
            except (RuntimeError, subprocess.TimeoutExpired):
                pass
            time.sleep(0.1)
        raise TimeoutError("server startup")

    def stop(sig):
        nonlocal server, log
        if server is not None and server.poll() is None:
            server.send_signal(sig)
            try:
                server.wait(timeout=30)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait(timeout=10)
                raise
        if log is not None:
            log.close()
        server = None

    try:
        init = subprocess.run([str(bins / "initdb"), "-D", str(data), "--no-locale", "--encoding=UTF8",
                               "-U", "rustsql", "--auth=trust"], env=env, capture_output=True, text=True, timeout=120)
        (root / "initdb.log").write_text(init.stdout + init.stderr)
        init.check_returncode()
        start()
        report["version"] = sql("SELECT version()")
        if "pgrust" not in report["version"]:
            raise AssertionError("wrong server: expected pgrust")
        check("durability_settings", "SELECT current_setting('fsync'), current_setting('synchronous_commit')", "on|on")
        sql("CREATE TABLE accounts(id integer PRIMARY KEY, balance integer NOT NULL); "
            "INSERT INTO accounts SELECT i, i FROM generate_series(1,20000) AS g(i)")
        check("heap_aggregate", "SELECT count(*), sum(balance) FROM accounts", "20000|200010000")
        check("rollback", "BEGIN; UPDATE accounts SET balance=0; ROLLBACK; SELECT sum(balance) FROM accounts", "200010000")
        check("savepoint", "BEGIN; SAVEPOINT s; DELETE FROM accounts; ROLLBACK TO s; COMMIT; SELECT count(*) FROM accounts", "20000")
        check("plan_invalidation", "PREPARE p(integer) AS SELECT balance FROM accounts WHERE id=$1; "
              "EXECUTE p(7); ALTER TABLE accounts ADD COLUMN note text; EXECUTE p(7)", "7\n7")
        sql("CREATE ACCESS METHOD pgrcolumnar TYPE TABLE HANDLER heap_tableam_handler; "
            "CREATE TABLE analytics(id integer, v integer) USING pgrcolumnar; "
            "INSERT INTO analytics SELECT i, i FROM generate_series(1,10000) AS g(i)")
        check("columnar_aggregate", "SELECT count(*), sum(v) FROM analytics", "10000|50005000")
        check("columnar_rollback", "BEGIN; UPDATE analytics SET v=0 WHERE id<=100; ROLLBACK; SELECT sum(v) FROM analytics", "50005000")
        sql("UPDATE analytics SET v=v+1 WHERE id<=100; DELETE FROM analytics WHERE id>9900")
        check("columnar_writes", "SELECT count(*), sum(v) FROM analytics", "9900|49010050")
        sql("CREATE TABLE counter(id integer PRIMARY KEY, n integer); INSERT INTO counter VALUES(1,0)")

        def writer(_):
            for _ in range(25):
                sql("UPDATE counter SET n=n+1 WHERE id=1", timeout=15)
        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
            list(pool.map(writer, range(4)))
        check("concurrent_committed_updates", "SELECT n FROM counter WHERE id=1", "100")
        stop(signal.SIGINT)
        start()
        check("clean_restart", "SELECT sum(balance) FROM accounts", "200010000")
        sql("UPDATE counter SET n=101 WHERE id=1")  # acknowledged commit before the injected crash
        stop(signal.SIGKILL)
        start()
        check("acknowledged_commit_survives_kill", "SELECT n FROM counter WHERE id=1", "101")
        check("columnar_survives_restart", "SELECT count(*), sum(v) FROM analytics", "9900|49010050")
        if args.mixed_seconds:
            from mixed_workload import run
            report["mixed"] = run(f"host={sock} port=55439 user=rustsql dbname=postgres", server.pid, args.mixed_seconds)
            if report["mixed"]["errors"]:
                raise AssertionError("mixed workload failed")
        suite_failed = False
        for name, directory, executable, schedule in (
            ("regression", args.regression_dir, "pg_regress", "parallel_schedule"),
            ("isolation", args.isolation_dir, "pg_isolation_regress", "isolation_schedule"),
        ):
            if directory is None:
                continue
            if args.reference_bin is None:
                raise ValueError("--reference-bin is required with a PostgreSQL suite")
            output = root / name
            output.mkdir()
            regdir = directory.resolve()
            database = "isolation_regression" if name == "isolation" else "regression"
            inputs = regdir
            if name == "isolation" and args.isolation_overlay:
                inputs = root / "isolation-input"
                inputs.mkdir()
                for subdir, suffix in (("specs", ".spec"), ("expected", ".out")):
                    shutil.copytree(regdir / subdir, inputs / subdir)
                    for fixture in (args.isolation_overlay / subdir).glob(f"*{suffix}"):
                        shutil.copyfile(fixture, inputs / subdir / fixture.name)
                (inputs / schedule).write_text((regdir / schedule).read_text() + "\n" +
                                               (args.isolation_overlay / "schedule.extra").read_text())
            cmd = [str(regdir / executable), f"--inputdir={inputs}", f"--outputdir={output}",
                   f"--bindir={args.reference_bin.resolve()}", f"--dlpath={regdir}", f"--host={sock}",
                   "--port=55439", "--user=rustsql", f"--dbname={database}", f"--schedule={inputs / schedule}"]
            with (root / f"{name}.log").open("w") as reglog:
                result = subprocess.run(cmd, cwd=regdir, env=env, stdout=reglog, stderr=reglog, timeout=900)
            report[name] = {"returncode": result.returncode, "directory": str(output),
                            "profile": "engine-overlay" if inputs != regdir else "postgresql-raw"}
            suite_failed |= result.returncode != 0
        if suite_failed:
            raise AssertionError(f"PostgreSQL suites failed: see logs in {root}")
        report["passed"] = True
    except Exception as exc:
        report["error"] = str(exc)
    finally:
        try:
            stop(signal.SIGINT)
        finally:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())

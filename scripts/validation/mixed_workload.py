"""Small mixed-load probe for server_smoke's disposable cluster; requires psycopg 3.

Includes client/protocol overhead. This is not ClickBench or a production capacity estimate.
"""
import math
import multiprocessing
import os
from pathlib import Path
import threading
import time


def run(dsn, pid, seconds=10, clients=6):
    import psycopg
    expected = [(b, sum(i + (i <= 100) for i in range(1, 9901) if i % 32 == b)) for b in range(32)]
    with psycopg.connect(dsn, autocommit=True) as conn:
        conn.execute("CREATE TABLE bench_accounts(id integer PRIMARY KEY, balance integer NOT NULL)")
        conn.execute("INSERT INTO bench_accounts SELECT i,0 FROM generate_series(1,10000) g(i)")
        conn.execute("CREATE ROLE bench_user LOGIN")
        conn.execute("GRANT SELECT, UPDATE ON bench_accounts TO bench_user")
        conn.execute("GRANT SELECT ON analytics TO bench_user")
    from psycopg.conninfo import make_conninfo
    app_dsn = make_conninfo(dsn, user="bench_user")

    def processes():
        # postgres has a supervisor and a separate server child. Count the tree,
        # not just Popen.pid, and never count individual threads as processes.
        todo, found = [pid], set()
        while todo:
            current = todo.pop()
            if current in found:
                continue
            found.add(current)
            todo.extend(int(x) for x in Path(f"/proc/{current}/task/{current}/children").read_text().split())
        return found

    server_pids = processes()
    def cpu():
        ticks = 0
        for current in server_pids:
            fields = Path(f"/proc/{current}/stat").read_text().rpartition(")")[2].split()
            ticks += int(fields[11]) + int(fields[12])
        return ticks / os.sysconf("SC_CLK_TCK")

    rss = []
    done = threading.Event()
    def sample():
        while not done.is_set():
            try:
                total = 0
                for current in server_pids:
                    for line in Path(f"/proc/{current}/status").read_text().splitlines():
                        if line.startswith("VmRSS:"):
                            total += int(line.split()[1]) * 1024
                rss.append(total)
            except FileNotFoundError:
                return
            done.wait(0.05)

    context = multiprocessing.get_context("fork")
    barrier = context.Barrier(clients + 1, timeout=30)
    begin_work = context.Event()
    deadline = context.Value("d", 0)
    completed = context.Queue()
    def worker(index):
        latencies, errors, updates = [], [], 0
        analytic = index >= clients - 2
        with psycopg.connect(app_dsn, autocommit=True) as conn:
            conn.execute("SET statement_timeout = '5s'")
            # Each connection exists before the measurement begins.
            barrier.wait()
            begin_work.wait(30)
            n = 0
            while time.monotonic() < deadline.value:
                begin = time.perf_counter()
                try:
                    if analytic:
                        rows = conn.execute("SELECT id%32, sum(v) FROM analytics GROUP BY 1 ORDER BY 1").fetchall()
                        if rows != expected:
                            raise AssertionError("analytical result mismatch")
                    elif n % 4 == 0:
                        key = (n * 17 + index * 1009) % 10000 + 1
                        cur = conn.execute("UPDATE bench_accounts SET balance=balance+1 WHERE id=%s", (key,))
                        if cur.rowcount != 1:
                            raise AssertionError("update affected wrong row count")
                        updates += 1
                    else:
                        key = (n * 17 + index * 1009) % 10000 + 1
                        if conn.execute("SELECT balance FROM bench_accounts WHERE id=%s", (key,)).fetchone() is None:
                            raise AssertionError("point lookup lost a row")
                    latencies.append((time.perf_counter() - begin) * 1000)
                except Exception as exc:
                    errors.append(str(exc))
                    break
                n += 1
        completed.put(("olap" if analytic else "oltp", latencies, updates, errors))

    monitor = threading.Thread(target=sample, daemon=True)
    workers = [context.Process(target=worker, args=(i,)) for i in range(clients)]
    try:
        # Fork before starting the sampler thread; clients do not share Python's GIL.
        for worker_process in workers:
            worker_process.start()
        barrier.wait()
        monitor.start()
        start_cpu = cpu()
        start = time.monotonic()
        deadline.value = start + seconds
        begin_work.set()
        results = [completed.get(timeout=seconds + 30) for _ in workers]
        elapsed, cpu_seconds = time.monotonic() - start, cpu() - start_cpu
    finally:
        done.set()
        if monitor.ident is not None:
            monitor.join()
        for worker_process in workers:
            if worker_process.pid is not None:
                worker_process.join(timeout=5)
                if worker_process.is_alive():
                    worker_process.terminate()
                    worker_process.join(timeout=5)
    updates = sum(r[2] for r in results)
    with psycopg.connect(dsn, autocommit=True) as conn:
        total = conn.execute("SELECT sum(balance) FROM bench_accounts").fetchone()[0]
        if total != updates:
            raise AssertionError(f"acknowledged updates: {updates}; stored increments: {total}")
    report = {"seconds": elapsed, "clients": clients, "server_cpu_seconds": cpu_seconds,
              "server_pids": sorted(server_pids), "sampled_peak_rss_sum_bytes": max(rss, default=0), "committed_updates": updates,
              "errors": [e for r in results for e in r[3]], "classes": {}}
    count = 0
    for kind in ("oltp", "olap"):
        values = sorted(v for r in results if r[0] == kind for v in r[1])
        count += len(values)
        report["classes"][kind] = {"operations": len(values), "ops_per_second": len(values) / elapsed,
            **{f"p{p}_ms": values[max(0, math.ceil(len(values) * p / 100) - 1)] if values else None for p in (50, 95, 99)}}
    report["cpu_seconds_per_operation"] = cpu_seconds / count if count else None
    return report

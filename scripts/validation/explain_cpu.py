#!/usr/bin/env python3
"""Paired EXPLAIN ANALYZE microbenchmark, not a mixed-workload performance claim."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import statistics
import time
import psycopg
from cluster import Cluster


SQL = "SELECT id,g,payload FROM explain_bench ORDER BY g,id LIMIT 17"


def sample(binaries, work_dir, iterations):
    with (binaries / "postgres").open("rb") as binary:
        digest = hashlib.file_digest(binary, "sha256").hexdigest()
    with Cluster(binaries, work_dir) as server, psycopg.connect(server.dsn, autocommit=True) as connection:
        connection.execute("CREATE TABLE explain_bench AS SELECT i id,i/10 g,repeat('x',256) payload FROM generate_series(1,10000) i")
        connection.execute("CREATE INDEX explain_bench_g ON explain_bench(g)")
        connection.execute("ANALYZE explain_bench; SET enable_seqscan=off; SET enable_bitmapscan=off; SET enable_sort=off")
        assert [r[0] for r in connection.execute(SQL)] == list(range(1, 18))
        query = "EXPLAIN(ANALYZE,FORMAT JSON,TIMING OFF,SUMMARY OFF) " + SQL
        plan = connection.execute(query, prepare=False).fetchone()[0][0]["Plan"]
        assert plan["Actual Rows"] == 17 and plan["Plans"][0]["Node Type"] == "Incremental Sort", plan
        for _ in range(200):
            connection.execute(query, prepare=False).fetchone()
        todo, processes = [server.process.pid], set()
        while todo:
            pid = todo.pop()
            if pid not in processes:
                processes.add(pid)
                todo.extend(map(int, Path(f"/proc/{pid}/task/{pid}/children").read_text().split()))
        ticks_per_second = os.sysconf("SC_CLK_TCK")
        def cpu_ticks():
            ticks = 0
            for pid in processes:
                fields = Path(f"/proc/{pid}/stat").read_text().rpartition(")")[2].split()
                ticks += int(fields[11]) + int(fields[12])
            return ticks
        latency = []
        first_cpu, start = cpu_ticks(), time.perf_counter()
        for _ in range(iterations):
            begin = time.perf_counter()
            connection.execute(query, prepare=False).fetchone()
            latency.append(time.perf_counter() - begin)
        elapsed, used_ticks = time.perf_counter() - start, cpu_ticks() - first_cpu
        latency.sort()
        return {"cluster": str(server.root), "sha256": digest, "version": server.version,
                "iterations": iterations, "server_cpu_ticks": used_ticks, "ticks_per_second": ticks_per_second,
                "server_cpu_seconds": used_ticks / ticks_per_second, "elapsed_seconds": elapsed,
                "p50_ms": statistics.median(latency) * 1000, "p95_ms": latency[int(.95 * (iterations - 1))] * 1000,
                "plan": plan}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("baseline", "candidate", "work-dir", "output"):
        parser.add_argument("--" + name, required=True, type=Path)
    parser.add_argument("--iterations", type=int, default=5000)
    args = parser.parse_args()
    if args.iterations < 1:
        parser.error("--iterations must be positive")
    report = {"scope": "warm EXPLAIN ANALYZE incremental-sort LIMIT; server process-tree CPU; client latency includes JSON decoding",
              "query": SQL, "order": ["baseline", "candidate", "candidate", "baseline"], "samples": []}
    for name in report["order"]:
        result = sample(getattr(args, name), args.work_dir, args.iterations)
        report["samples"].append(dict(name=name, **result))
        print(json.dumps({k: v for k, v in report["samples"][-1].items() if k != "plan"}), flush=True)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()

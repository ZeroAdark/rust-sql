#!/usr/bin/env python3
"""Check bounded pre-auth connection queues in a private disposable cluster."""
import argparse
import asyncio
from concurrent.futures import ThreadPoolExecutor
from contextlib import ExitStack
import hashlib
import json
import os
import platform
from pathlib import Path
import re
import resource
import socket
import struct
import time

import psycopg
from cluster import Cluster


class Startup:
    """A protocol 3.0 startup, including fatal SQLSTATEs libpq may not expose."""
    def __init__(self, cluster, user="queue_user", *, bypass=False, application="queue-probe"):
        self.socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.socket.settimeout(8)
        self.socket.connect(str(cluster.socket / ".s.PGSQL.55440"))
        parameters = {"user": user, "database": "postgres", "application_name": application}
        if bypass:
            parameters["options"] = "-c pgrust.admission_bypass=on"
        payload = struct.pack("!I", 196608) + b"".join(k.encode() + b"\0" + v.encode() + b"\0"
                                                     for k, v in parameters.items()) + b"\0"
        self.started = time.monotonic()
        self.socket.sendall(struct.pack("!I", len(payload) + 4) + payload)

    def exact(self, size):
        data = bytearray()
        while len(data) < size:
            chunk = self.socket.recv(size - len(data))
            if not chunk:
                raise EOFError("server closed during startup")
            data.extend(chunk)
        return bytes(data)

    def finish(self, allow_auth_eof=False):
        while True:
            try:
                kind = self.exact(1)
            except EOFError:
                if not allow_auth_eof:
                    raise
                # PostgreSQL ProcessInterrupts suppresses remote output during
                # authentication when terminating a backend (postgres.c:3311).
                return {"ready": False, "auth_eof": True, "seconds": time.monotonic() - self.started}
            size = struct.unpack("!I", self.exact(4))[0]
            if not 4 <= size <= 16 * 1024 * 1024:
                raise ValueError("invalid startup frame length")
            payload = self.exact(size - 4)
            if kind == b"E":
                fields = {chr(v[0]): v[1:].decode("utf8", "replace") for v in payload.split(b"\0") if v}
                self.close()
                return {"ready": False, "sqlstate": fields.get("C"), "message": fields.get("M"),
                        "seconds": time.monotonic() - self.started}
            if kind == b"Z":
                return {"ready": True, "transaction_status": payload.decode(),
                        "seconds": time.monotonic() - self.started}

    def close(self):
        self.socket.close()

    async def message_async(self):
        loop = asyncio.get_running_loop()

        async def exact(size):
            data = bytearray()
            while len(data) < size:
                chunk = await loop.sock_recv(self.socket, size - len(data))
                if not chunk:
                    raise EOFError("connection closed during burst")
                data.extend(chunk)
            return bytes(data)

        kind = await exact(1)
        size = struct.unpack("!I", await exact(4))[0]
        if not 4 <= size <= 16 * 1024 * 1024:
            raise ValueError("invalid server frame length")
        return kind, await exact(size - 4)

    async def finish_async(self):
        while True:
            kind, payload = await self.message_async()
            if kind == b"E":
                fields = {chr(v[0]): v[1:].decode("utf8", "replace") for v in payload.split(b"\0") if v}
                return {"ready": False, "sqlstate": fields.get("C"), "message": fields.get("M")}
            if kind == b"Z":
                return {"ready": True, "seconds": time.monotonic() - self.started}

    async def stats_async(self):
        payload = b"pgrust: admission stats\0"
        await asyncio.get_running_loop().sock_sendall(self.socket, b"Q" + struct.pack("!I", len(payload) + 4) + payload)
        state = None
        while True:
            kind, payload = await self.message_async()
            if kind == b"E":
                raise RuntimeError("admission stats failed during burst")
            if kind == b"N":
                fields = {chr(v[0]): v[1:].decode("utf8", "replace") for v in payload.split(b"\0") if v}
                if fields.get("M", "").startswith("admission stats:"):
                    state = {k: int(v) for k, v in re.findall(r"(\w+)=(\d+)", fields["M"])}
            if kind == b"Z":
                if state is None:
                    raise RuntimeError("missing admission stats during burst")
                return state


async def burst_wave(cluster, group, wave, server_cpus):
    """Hold 500 admitted startups and 300 queued startups, then drain all."""
    from resource_scope import snapshot
    clients, tasks = [], []
    started = time.monotonic()
    try:
        for _ in range(800):
            # Connect/send startup with bounded socket waits, without waiting
            # for authentication. All 800 sockets remain open together.
            client = Startup(cluster)
            clients.append(client)
            client.socket.setblocking(False)
            tasks.append(asyncio.create_task(client.finish_async()))
            if len(tasks) % 32 == 0:
                await asyncio.sleep(0)
        deadline = time.monotonic() + 15
        while sum(task.done() for task in tasks) < 500 and time.monotonic() < deadline:
            await asyncio.sleep(0.01)
        ready = [i for i, task in enumerate(tasks) if task.done()]
        results = [tasks[i].result() for i in ready]
        if len(ready) != 500 or not all(r["ready"] for r in results):
            raise AssertionError({"initial_completed": len(ready), "errors": [r for r in results if not r["ready"]]})
        monitor = clients[ready[0]]
        deadline = time.monotonic() + 5
        while True:
            before = await monitor.stats_async()
            if before["queued"] == 300:
                break
            if time.monotonic() >= deadline:
                raise TimeoutError(before)
            await asyncio.sleep(0.01)
        assert before["total_queued"] == before["served"] + 300
        assert before["timeouts"] == 0 and before["hangups"] == 0
        held = snapshot(group)
        held["pids_current"] = int((group / "pids.current").read_text())
        masks = {}
        for pid in (group / "cgroup.procs").read_text().split():
            for task in (Path("/proc") / pid / "task").iterdir():
                mask = os.sched_getaffinity(int(task.name))
                assert mask and mask <= server_cpus, (task, mask, server_cpus)
                key = ",".join(map(str, sorted(mask)))
                masks[key] = masks.get(key, 0) + 1
        held["thread_affinity_counts"] = masks
        overflow = Startup(cluster)
        try:
            overflow.socket.setblocking(False)
            rejection = await overflow.finish_async()
            assert rejection.get("sqlstate") == "53300", rejection
        finally:
            overflow.close()
        # Keep one admitted client to observe the queue while the other 499
        # slots are returned. No additional admin connection consumes a slot.
        for i in ready[1:]:
            clients[i].close()
        pending = [task for task in tasks if not task.done()]
        admitted = await asyncio.wait_for(asyncio.gather(*pending), timeout=15)
        assert len(admitted) == 300 and all(r["ready"] for r in admitted), admitted
        after = await monitor.stats_async()
        assert after["queued"] == 0 and after["served"] == before["served"] + 300
        assert after["total_queued"] == before["total_queued"]
        assert after["timeouts"] == 0 and after["hangups"] == 0
        return {"wave": wave, "passed": True, "accepted": 800, "initially_ready": 500,
                "initially_queued": 300, "overflow_rejection": rejection, "stats_before": before,
                "stats_after": after, "resources_while_held": held, "seconds": time.monotonic() - started,
                "startup_seconds": sorted(task.result()["seconds"] for task in tasks)}
    finally:
        for client in clients:
            client.close()
        for task in tasks:
            if not task.done():
                task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)


def run_burst(args, report):
    from resource_scope import DelegatedScope, Sampler, delta, snapshot
    if not __debug__:
        raise RuntimeError("burst validation requires Python assertions")
    soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
    if hard < 4096:
        raise RuntimeError("burst requires an open-file hard limit of at least 4096")
    resource.setrlimit(resource.RLIMIT_NOFILE, (max(soft, 4096), hard))
    scope = DelegatedScope(args.server_cpus, args.client_cpus)
    group, launcher = scope.server(args.memory_mib)
    report.update(mode="burst", binary_sha256=hashlib.sha256((args.candidate / "postgres").read_bytes()).hexdigest(),
                  assertions_enabled=__debug__, server_cpus=sorted(scope.server_cpus), client_cpus=sorted(scope.client_cpus),
                  libc=platform.libc_ver(), glibc_tunables=os.environ.get("GLIBC_TUNABLES", ""),
                  transparent_hugepage=(Path("/sys/kernel/mm/transparent_hugepage/enabled").read_text().strip()),
                  source_lf_sha256={name: hashlib.sha256(Path(__file__).with_name(name).read_bytes().replace(b"\r\n", b"\n")).hexdigest()
                                   for name in ("connection_queue_probe.py", "cluster.py", "resource_scope.py")},
                  cgroup=str(group), limits={n: (group / n).read_text().strip()
                  for n in ("memory.max", "memory.swap.max", "cpu.max", "pids.max")})
    before = None
    try:
        with Cluster(args.candidate, args.work_dir, launcher=launcher,
                     settings=["max_connections=500", "reserved_connections=0", "superuser_reserved_connections=0",
                               "connection_queue_size=300", "connection_queue_timeout=20000"]) as cluster:
            report.update(cluster=str(cluster.root), version=cluster.version)
            assert cluster.process.pid in {int(pid) for pid in (group / "cgroup.procs").read_text().split()}
            assert os.sched_getaffinity(cluster.process.pid) == scope.server_cpus
            with psycopg.connect(cluster.dsn, autocommit=True) as admin:
                admin.execute("CREATE ROLE queue_user LOGIN")
            before = snapshot(group)
            report["resources_before"] = before
            sampler = Sampler(group)
            sampler.start()
            report["waves"] = []
            try:
                for wave in (1, 2):
                    result = asyncio.run(asyncio.wait_for(burst_wave(cluster, group, wave, scope.server_cpus), timeout=45))
                    report["waves"].append(result)
                    print(json.dumps({k: v for k, v in result.items() if k != "startup_seconds"}), flush=True)
            finally:
                report["sampled_resources"] = sampler.stop()
                after = snapshot(group)
                report.update(resources_after=after, cpu_delta=delta(before["cpu"], after["cpu"]),
                              memory_events=delta(before["events"], after["events"]))
        report["resources_after_shutdown"] = snapshot(group)
        report["cpu_delta_including_shutdown"] = delta(before["cpu"], report["resources_after_shutdown"]["cpu"])
        assert not any(report["resources_after_shutdown"]["events"].get(k, 0) for k in ("oom", "oom_kill", "oom_group_kill"))
        assert report["resources_after_shutdown"]["memory_swap_current"] == 0
        assert report["sampled_resources"]["samples"] > 0 and report["sampled_resources"]["incomplete_process_samples"] == 0
        assert len(report["waves"]) == 2 and all(w["passed"] for w in report["waves"])
        report["passed"] = True
    finally:
        # Preserve the kernel counters even when an OOM/reset aborts a wave.
        report["resources_after_shutdown"] = snapshot(group)
        if before is not None:
            report["cpu_delta_including_shutdown"] = delta(before["cpu"], report["resources_after_shutdown"]["cpu"])
        report["remaining_server_pids"] = (group / "cgroup.procs").read_text().split()
        if report["remaining_server_pids"]:
            report["passed"] = False
            raise RuntimeError("private server cgroup remains populated")
        group.rmdir()


def run(args, report):
    with Cluster(args.candidate, args.work_dir, settings=["max_connections=10", "reserved_connections=1",
                 "superuser_reserved_connections=2", "connection_queue_size=2", "connection_queue_timeout=1400"]) as cluster, ExitStack() as stack:
        report.update(cluster=str(cluster.root), version=cluster.version,
                      binary_sha256=hashlib.sha256((args.candidate / "postgres").read_bytes()).hexdigest())
        admin = stack.enter_context(psycopg.connect(cluster.dsn, autocommit=True))
        admin.execute("CREATE ROLE queue_user LOGIN; CREATE ROLE queue_reserved LOGIN; "
                      "GRANT pg_use_reserved_connections TO queue_reserved", prepare=False)
        notes = []
        admin.add_notice_handler(lambda d: notes.append(d.message_primary))

        def stats():
            notes.clear()
            admin.execute("pgrust: admission stats", prepare=False)
            entries = [s for s in notes if s.startswith("admission stats:")]
            if len(entries) != 1:
                raise RuntimeError("missing admission counters")
            return {k: int(v) for k, v in re.findall(r"(\w+)=(\d+)", entries[0])}

        def wait_for(predicate, seconds=2):
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                state = stats()
                if predicate(state):
                    return state
                time.sleep(0.005)
            raise TimeoutError(f"queue condition not reached: {stats()}")

        def connect(**kwargs):
            client = Startup(cluster, **kwargs)
            stack.callback(client.close)
            return client

        def check(name, passed, **details):
            item = {"name": name, "passed": bool(passed), **details}
            report["checks"].append(item)
            print(json.dumps(item), flush=True)
            if not passed:
                raise AssertionError(name)

        holders = [connect() for _ in range(6)]
        check("ordinary_capacity", all(c.finish()["ready"] for c in holders) and stats()["queued"] == 0)
        with ThreadPoolExecutor(max_workers=3) as pool:
            first = connect()
            a = pool.submit(first.finish)
            wait_for(lambda s: s["queued"] == 1)
            second = connect()
            b = pool.submit(second.finish)
            state = wait_for(lambda s: s["queued"] == 2)
            overflow = connect().finish()
            check("bounded_queue_rejects_overflow", overflow.get("sqlstate") == "53300" and overflow["seconds"] < 1,
                  queued=state, rejection=overflow)

            for name, kwargs in (("option", {"bypass": True}), ("application", {"application": "psql-forged"})):
                result = connect(**kwargs).finish()
                check(f"unprivileged_{name}_claim_denied", result.get("sqlstate") == "53300", result=result)
            reserved = connect(user="queue_reserved", bypass=True)
            check("reserved_role_connects", reserved.finish()["ready"])
            result = connect(user="queue_reserved", bypass=True).finish()
            check("reserved_role_cannot_take_superuser_band", result.get("sqlstate") == "53300", result=result)
            superuser = connect(user="rustsql", bypass=True)
            check("superuser_connects_while_queued", superuser.finish()["ready"])
            superuser.close()
            reserved.close()
            holders.pop().close()
            deadline = time.monotonic() + 1
            while not (a.done() or b.done()) and time.monotonic() < deadline:
                time.sleep(0.005)
            done, pending = (a, b) if a.done() else (b, a)
            admitted = first if done is a else second
            result = done.result(timeout=2)
            check("released_slot_serves_one", result["ready"] and not pending.done() and stats()["queued"] == 1, result=result)
            admitted.close()
            result = pending.result(timeout=2)
            check("queue_drains", result["ready"] and stats()["queued"] == 0, result=result)

            before = stats()
            timeout = connect().finish()
            after = stats()
            check("queue_timeout_sqlstate", timeout.get("sqlstate") == "53300" and 1.2 <= timeout["seconds"] < 3
                  and after["timeouts"] == before["timeouts"] + 1 and after["queued"] == 0,
                  result=timeout, after=after)
            abandoned = connect()
            wait_for(lambda s: s["queued"] == 1)
            before = stats()
            abandoned.close()
            after = wait_for(lambda s: s["queued"] == 0 and s["hangups"] == before["hangups"] + 1)
            check("disconnected_waiter_reaped", after["queued"] == 0, after=after)

            # Shutdown must interrupt pre-auth waiters rather than admitting
            # them into slots freed by the shutdown cascade.
            pending = [connect(), connect()]
            futures = [pool.submit(c.finish, True) for c in pending]
            wait_for(lambda s: s["queued"] == 2)
            report["stats_before_shutdown"] = stats()
            started = time.monotonic()
            cluster.stop()
            results = [f.result(timeout=3) for f in futures]
            check("shutdown_interrupts_queued_startups", all(not r["ready"] and
                  (r.get("auth_eof") or r.get("sqlstate") == "57P01") for r in results),
                  results=results, shutdown_seconds=time.monotonic() - started)
        report["passed"] = all(c["passed"] for c in report["checks"])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--burst", action="store_true", help="two 800-connection waves in a private delegated cgroup")
    parser.add_argument("--memory-mib", type=int, default=4096)
    parser.add_argument("--server-cpus", type=int, nargs="+", default=[0, 2, 4, 6])
    parser.add_argument("--client-cpus", type=int, nargs="+", default=[8, 10])
    args = parser.parse_args()
    report = {"passed": False, "checks": [],
              "harness_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}
    try:
        if args.burst:
            run_burst(args, report)
        else:
            run(args, report)
    except BaseException as error:
        report.update(passed=False, exception=repr(error))
        raise
    finally:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()

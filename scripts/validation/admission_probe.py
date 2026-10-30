#!/usr/bin/env python3
"""Exercise transaction admission with real protocol messages and ordinary roles.

Only creates private, disposable clusters. Retains failures and server logs.
The raw client makes Parse, Bind, Describe, Execute, Flush, Sync and fast-path
entry points explicit; a driver must not silently choose a different protocol.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor, TimeoutError as FutureTimeout
from contextlib import ExitStack
import hashlib
import json
from pathlib import Path
import re
import socket
import struct
import time

import psycopg
from cluster import Cluster


def frame(kind, payload=b""):
    return kind + struct.pack("!I", len(payload) + 4) + payload


def parse(query, name="", parameter=False):
    return frame(b"P", name.encode() + b"\0" + query.encode() + b"\0" +
                 (struct.pack("!HI", 1, 23) if parameter else b"\0\0"))


def bind(name="", portal="", parameter=False):
    values = struct.pack("!HI", 1, 1) + b"1" if parameter else b"\0\0"
    return frame(b"B", portal.encode() + b"\0" + name.encode() + b"\0\0\0" + values + b"\0\0")


EXECUTE = frame(b"E", b"\0\0\0\0\0")
SYNC = frame(b"S")
FLUSH = frame(b"H")
SQL = "SELECT value FROM admission_data WHERE id=1"


class Wire:
    def __init__(self, cluster, timeout=4000, transaction_timeout=0):
        self.socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.socket.settimeout(10)
        self.socket.connect(str(cluster.socket / ".s.PGSQL.55440"))
        startup = (b"user\0admission_user\0database\0postgres\0application_name\0admission-probe\0options\0" +
                   f"-c statement_timeout={timeout} -c transaction_timeout={transaction_timeout}".encode() + b"\0\0")
        payload = struct.pack("!I", 196608) + startup
        self.socket.sendall(struct.pack("!I", len(payload) + 4) + payload)
        self.pid = None
        self.key = None
        self.read()

    def close(self):
        self.socket.close()  # Deliberate EOF also checks backend-exit cleanup.

    def exact(self, size):
        chunks = bytearray()
        while len(chunks) < size:
            data = self.socket.recv(size - len(chunks))
            if not data:
                raise EOFError("server closed protocol connection")
            chunks.extend(data)
        return bytes(chunks)

    def read(self, until=b"Z"):
        result = {"tags": [], "rows": [], "errors": []}
        while True:
            kind = self.exact(1)
            size = struct.unpack("!I", self.exact(4))[0]
            if not 4 <= size <= 16 * 1024 * 1024:
                raise ValueError(f"invalid server frame length: {size}")
            payload = self.exact(size - 4)
            result["tags"].append(kind.decode("ascii"))
            if kind == b"E":
                fields = {chr(field[0]): field[1:].decode("utf8", "replace")
                          for field in payload.split(b"\0") if field}
                result["errors"].append({"sqlstate": fields.get("C"), "message": fields.get("M")})
            elif kind == b"K":
                self.pid = struct.unpack("!I", payload[:4])[0]
                self.key = payload
            elif kind == b"D":
                count = struct.unpack("!H", payload[:2])[0]
                offset, row = 2, []
                for _ in range(count):
                    length = struct.unpack("!i", payload[offset:offset + 4])[0]
                    offset += 4
                    row.append(None if length == -1 else payload[offset:offset + length].decode())
                    if length != -1:
                        offset += length
                result["rows"].append(row)
            elif kind == b"V":
                result["function_result_hex"] = payload.hex()
            if kind == until:
                if kind == b"Z":
                    result["transaction_status"] = payload.decode()
                return result

    def exchange(self, *messages, until=b"Z"):
        self.socket.sendall(b"".join(messages))
        return self.read(until)

    def query(self, sql):
        return self.exchange(frame(b"Q", sql.encode() + b"\0"))

    def cancel(self, cluster):
        if self.key is None or len(self.key) != 8:
            raise RuntimeError("missing protocol 3.0 cancellation key")
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as cancel:
            cancel.settimeout(3)
            cancel.connect(str(cluster.socket / ".s.PGSQL.55440"))
            cancel.sendall(struct.pack("!II", 16, 80877102) + self.key)
            if cancel.recv(1) != b"":
                raise RuntimeError("CancelRequest unexpectedly returned data")


def run(args, report):
    with Cluster(args.candidate, args.work_dir,
                 settings=["max_prepared_transactions=8"]) as cluster, ExitStack() as stack:
        report["cluster"], report["version"] = str(cluster.root), cluster.version
        report["binary_sha256"] = hashlib.sha256((Path(args.candidate) / "postgres").read_bytes()).hexdigest()
        admin = stack.enter_context(psycopg.connect(cluster.dsn, autocommit=True))
        admin.execute("CREATE ROLE admission_user LOGIN; CREATE TABLE admission_data(id int PRIMARY KEY,value int); "
                      "INSERT INTO admission_data VALUES(1,42); GRANT ALL ON admission_data TO admission_user", prepare=False)
        notes = []
        admin.add_notice_handler(lambda diagnostic: notes.append(diagnostic.message_primary))

        def limit(value):
            # A command-line -c setting would override ALTER SYSTEM and make
            # a reload test meaningless. Use the reloadable config source.
            admin.execute(f"ALTER SYSTEM SET max_active_queries={value}")
            admin.execute("SELECT pg_reload_conf()")
            deadline = time.monotonic() + 3
            while time.monotonic() < deadline:
                if admin.execute("SHOW max_active_queries").fetchone()[0] == str(value):
                    return
                time.sleep(0.01)
            raise TimeoutError(f"max_active_queries did not reload to {value}")

        limit(1)

        def stats():
            notes.clear()
            admin.execute("pgrust: admission stats", prepare=False)
            matches = [note for note in notes if note.startswith("admission stats:")]
            if len(matches) != 1:
                raise RuntimeError(f"missing admission counters: {notes}")
            return {key: int(value) for key, value in re.findall(r"(\w+)=(\d+)", matches[0])}

        def check(name, passed, **details):
            item = {"name": name, "passed": bool(passed), **details}
            report["checks"].append(item)
            print(json.dumps(item), flush=True)

        def wire(**kwargs):
            client = Wire(cluster, **kwargs)
            stack.callback(client.close)
            return client

        def ok(result):
            if result["errors"]:
                raise RuntimeError(result)
            return result

        def wait_for(predicate, future=None, seconds=2):
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                state = stats()
                if predicate(state):
                    return state
                if future is not None and future.done():
                    return state
                time.sleep(0.01)
            return stats()

        holder = wire()
        abs_oid = admin.execute("SELECT 'pg_catalog.int4abs(integer)'::regprocedure::oid").fetchone()[0]
        # FunctionCall: OID, one binary format, one argument, int32 length/value,
        # binary result format. Length is a full int32, not a format field.
        fastpath = frame(b"F", struct.pack("!IHHHIiH", abs_oid, 1, 1, 1, 4, -42, 1))
        with ThreadPoolExecutor(max_workers=16) as pool:
            for mode in ("simple", "parse", "parameterized", "bind", "describe_statement", "describe_portal", "fastpath"):
                for action in ("statement_timeout", "cancel"):
                    client = wire(timeout=450 if action == "statement_timeout" else 4000)
                    if mode in ("bind", "describe_statement"):
                        ok(client.exchange(parse(SQL, "saved"), SYNC))
                    if mode == "describe_portal":
                        ok(client.query("BEGIN; DECLARE kept CURSOR WITH HOLD FOR " + SQL + "; COMMIT"))
                    commands = {
                        "simple": (frame(b"Q", SQL.encode() + b"\0"),),
                        "parse": (parse(SQL, "queued"), SYNC),
                        "parameterized": (parse(SQL.replace("id=1", "id=$1"), parameter=True), bind(parameter=True), EXECUTE, SYNC),
                        "bind": (bind("saved"), EXECUTE, SYNC),
                        "describe_statement": (frame(b"D", b"Ssaved\0"), SYNC),
                        "describe_portal": (frame(b"D", b"Pkept\0"), SYNC),
                        "fastpath": (fastpath,),
                    }[mode]
                    ok(holder.query("BEGIN"))
                    before = stats()
                    started = time.monotonic()
                    future = pool.submit(client.exchange, *commands)
                    queued = wait_for(lambda s: s["waiters"] == 1, future)
                    locks = admin.execute("SELECT mode FROM pg_locks WHERE pid=%s AND relation='admission_data'::regclass",
                                          (client.pid,)).fetchall()
                    if action == "cancel" and not future.done():
                        admin.execute("SELECT pg_cancel_backend(%s)", (client.pid,))
                    result = future.result(timeout=6)
                    elapsed = time.monotonic() - started
                    after = stats()
                    passed = (queued["active"] == 1 and queued["waiters"] == 1 and not locks
                              and [e["sqlstate"] for e in result["errors"]] == ["57014"]
                              and after["active"] == 1 and after["waiters"] == 0
                              and after["total_waits"] == before["total_waits"] + 1)
                    check(f"{mode}_{action}", passed, queued=queued, after=after,
                          table_locks=locks, result=result, seconds=elapsed)
                    ok(holder.query("COMMIT"))
                    resumed = ok(client.query(SQL))
                    check(f"{mode}_{action}_reuse", resumed["rows"] == [["42"]] and stats()["active"] == 0)
                    client.close()

            direct = wire()
            result = ok(direct.exchange(fastpath))
            check("fastpath_binary_result", result.get("function_result_hex") == "000000040000002a"
                  and stats()["active"] == 0, result=result)

            # A burst must queue all ordinary sessions; wire cancellation
            # removes half without leaking slots or stranding the survivors.
            burst = [wire() for _ in range(16)]
            ok(holder.query("BEGIN"))
            before = stats()
            futures = [pool.submit(c.exchange, parse(SQL), bind(), EXECUTE, SYNC) for c in burst]
            queued = wait_for(lambda s: s["waiters"] == 16)
            for c in burst[:8]:
                c.cancel(cluster)
            cancelled = [f.result(timeout=3) for f in futures[:8]]
            remaining = stats()
            ok(holder.query("COMMIT"))
            completed = [ok(f.result(timeout=6)) for f in futures[8:]]
            after = stats()
            check("burst_cancel_and_drain", queued["active"] == 1 and queued["waiters"] == 16
                  and remaining["active"] == 1 and remaining["waiters"] == 8
                  and all([e["sqlstate"] for e in r["errors"]] == ["57014"] for r in cancelled)
                  and all(r["rows"] == [["42"]] for r in completed)
                  and after["active"] == 0 and after["waiters"] == 0
                  and after["total_waits"] == before["total_waits"] + 16,
                  queued=queued, remaining=remaining, after=after, cancelled=cancelled)
            for c in burst:
                c.close()

            # Parse + Flush starts a transaction without Execute or Sync.
            client = wire()
            result = ok(client.exchange(parse(SQL, "held"), FLUSH, until=b"1"))
            state = stats()
            check("parse_flush_holds_slot", state["active"] == 1, stats=state, result=result)
            ok(client.exchange(SYNC))
            check("parse_sync_releases_slot", stats()["active"] == 0)

            # A pipeline error must discard later messages up to Sync and release.
            result = client.exchange(parse("SELECT 1/0"), bind(), EXECUTE,
                                     parse("UPDATE admission_data SET value=999"), bind(), EXECUTE, SYNC)
            check("pipeline_error_sync", [e["sqlstate"] for e in result["errors"]] == ["22012"]
                  and stats()["active"] == 0 and ok(client.query(SQL))["rows"] == [["42"]], result=result)

            # A lock holder must continue while another transaction waits at admission.
            ok(holder.query("BEGIN; UPDATE admission_data SET value=43 WHERE id=1"))
            waiter = wire()
            future = pool.submit(waiter.query, "UPDATE admission_data SET value=value+1 WHERE id=1 RETURNING value")
            queued = wait_for(lambda s: s["waiters"] == 1, future)
            ok(holder.exchange(parse("UPDATE admission_data SET value=value+1 WHERE id=1"), bind(), EXECUTE, SYNC))
            ok(holder.query("COMMIT"))
            result = ok(future.result(timeout=6))
            check("lock_holder_continues", queued["waiters"] == 1 and result["rows"] == [["45"]]
                  and stats()["active"] == 0, result=result)

            ok(holder.exchange(parse("BEGIN"), bind(), EXECUTE, SYNC))
            result = holder.exchange(parse("SELECT 1/0"), bind(), EXECUTE, SYNC)
            check("explicit_abort_retains_slot", result["transaction_status"] == "E" and stats()["active"] == 1, result=result)
            ok(holder.exchange(parse("ROLLBACK"), bind(), EXECUTE, SYNC))
            check("explicit_rollback_releases_slot", stats()["active"] == 0)

            for ending in ("eof", "terminate"):
                dying = wire()
                ok(dying.exchange(parse(SQL), FLUSH, until=b"1"))
                future = pool.submit(waiter.query, SQL)
                queued = wait_for(lambda s: s["waiters"] == 1, future)
                if ending == "eof":
                    dying.close()
                else:
                    admin.execute("SELECT pg_terminate_backend(%s)", (dying.pid,))
                result = ok(future.result(timeout=6))
                check(f"{ending}_releases_slot", queued["waiters"] == 1 and result["rows"] == [["45"]]
                      and stats()["active"] == 0, result=result)

            # Prepared transactions retain row locks without a session slot.
            # Their finisher must bypass the gate while its admitted lock waiter runs.
            expected = 45
            for ending, mode in ((e, m) for m in ("parse", "bind", "describe") for e in ("COMMIT", "ROLLBACK")):
                finisher = wire()
                command = ending + " PREPARED 'admission_twophase'"
                if mode != "parse":
                    ok(finisher.exchange(parse(command, "finish"), SYNC))
                ok(holder.query("BEGIN; UPDATE admission_data SET value=value+10 WHERE id=1; PREPARE TRANSACTION 'admission_twophase'"))
                future = pool.submit(waiter.query, "UPDATE admission_data SET value=value+1 WHERE id=1 RETURNING value")
                state = wait_for(lambda s: s["active"] == 1, future)
                if mode == "parse":
                    messages = (parse(command), bind(), EXECUTE, SYNC)
                else:
                    prefix = (frame(b"D", b"Sfinish\0"),) if mode == "describe" else ()
                    messages = (*prefix, bind("finish"), EXECUTE, SYNC)
                result = ok(finisher.exchange(*messages))
                resumed = ok(future.result(timeout=6))
                expected += 11 if ending == "COMMIT" else 1
                check(f"{ending.lower()}_prepared_{mode}_bypass", state["active"] == 1 and not result["errors"]
                      and resumed["rows"] == [[str(expected)]]
                      and stats()["active"] == 0, finisher=result, resumed=resumed)

            # Session advisory lock owners similarly must remain able to unlock.
            ok(holder.query("SELECT pg_advisory_lock(123456)"))
            future = pool.submit(waiter.query, "SELECT pg_advisory_lock(123456)")
            state = wait_for(lambda s: s["active"] == 1, future)
            result = ok(holder.exchange(parse("SELECT pg_advisory_unlock(123456)"), bind(), EXECUTE, SYNC))
            ok(future.result(timeout=6))
            ok(waiter.query("SELECT pg_advisory_unlock(123456)"))
            check("session_lock_owner_bypass", state["active"] == 1 and result["rows"] == [["t"]] and stats()["active"] == 0)

            # A configured transaction timeout is not armed before admission.
            # It must not suppress the statement timer while waiting to begin.
            for mode in ("simple", "parameterized"):
                limited = wire(timeout=450, transaction_timeout=150)
                ok(holder.query("BEGIN"))
                commands = ((frame(b"Q", b"SELECT 42\0"),) if mode == "simple" else
                            (parse("SELECT $1::int", parameter=True), bind(parameter=True), EXECUTE, SYNC))
                future = pool.submit(limited.exchange, *commands)
                queued = wait_for(lambda s: s["waiters"] == 1, future)
                timed_out = True
                try:
                    result = future.result(timeout=1)
                except FutureTimeout:
                    timed_out = False
                    admin.execute("SELECT pg_cancel_backend(%s)", (limited.pid,))
                    result = future.result(timeout=3)
                check(f"{mode}_queue_timer_before_transaction", timed_out and queued["waiters"] == 1
                      and [e["sqlstate"] for e in result["errors"]] == ["57014"]
                      and "statement timeout" in result["errors"][0]["message"], result=result)
                ok(holder.query("COMMIT"))
                limited.close()

            ok(holder.query("BEGIN"))
            future = pool.submit(waiter.exchange, parse("BEGIN"), bind(), EXECUTE, SYNC)
            queued = wait_for(lambda s: s["waiters"] == 1, future)
            limit(2)
            ok(future.result(timeout=3))
            check("reload_raise_drains_waiter", queued["waiters"] == 1 and stats()["active"] == 2)
            limit(1)
            third = wire()
            future = pool.submit(third.query, "BEGIN")
            queued = wait_for(lambda s: s["waiters"] == 1, future)
            ok(holder.query("COMMIT"))
            check("reload_lower_retains_owners", queued["active"] == 2 and stats()["active"] == 1 and not future.done())
            ok(waiter.query("COMMIT"))
            ok(future.result(timeout=3))
            check("reload_lower_then_drains", stats()["active"] == 1 and stats()["waiters"] == 0)
            ok(third.query("COMMIT"))
            ok(holder.query("BEGIN"))
            future = pool.submit(waiter.query, "BEGIN")
            queued = wait_for(lambda s: s["waiters"] == 1, future)
            limit(0)
            ok(future.result(timeout=3))
            ok(holder.query("COMMIT"))
            ok(waiter.query("COMMIT"))
            check("reload_disable_balances_slots", queued["waiters"] == 1 and stats()["active"] == 0 and stats()["waiters"] == 0)
            limit(1)

        report["final_stats"] = stats()
        check("final_zero_occupancy", all(report["final_stats"][k] == 0 for k in ("active", "waiters", "queued")))
        report["passed"] = all(item["passed"] for item in report["checks"])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    report = {"passed": False, "checks": [],
              "harness_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}
    try:
        run(args, report)
    except BaseException as error:
        report["passed"] = False
        report["exception"] = repr(error)
        raise
    finally:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2) + "\n")
    raise SystemExit(0 if report["passed"] else 1)


if __name__ == "__main__":
    main()

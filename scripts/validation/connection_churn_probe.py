#!/usr/bin/env python3
"""Exercise connection churn alongside durable OLTP/OLAP in an owned cgroup.

This is a correctness/pressure campaign, not a baseline performance comparison.
Each wave holds 500 admitted and 300 queued startups while three ordinary-role
sessions keep working. An administrative session remains available for checks.
"""
import argparse
import asyncio
from concurrent.futures import ThreadPoolExecutor
from contextlib import ExitStack
import hashlib
import json
import os
from pathlib import Path
import resource
import threading
import time

import psycopg

from cluster import Cluster
from connection_queue_probe import burst_wave
from resource_scope import DelegatedScope, Sampler, delta, snapshot


class CheckedCluster(Cluster):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.shutdown_exit_codes = []

    def stop(self, crash=False):
        process = self.process
        try:
            super().stop(crash=crash)
        finally:
            if process is not None:
                self.shutdown_exit_codes.append(process.returncode)
        if process is not None and not crash and process.returncode != 0:
            raise RuntimeError(f'normal shutdown returned {process.returncode}; see {self.root}')


def save(path, report):
    temporary = path.with_suffix(path.suffix + '.tmp')
    temporary.write_text(json.dumps(report, indent=2) + '\n')
    temporary.replace(path)


def expected_groups(rows):
    result = [[0, 0, 0] for _ in range(97)]
    for i in range(1, rows + 1):
        group = result[i % 97]
        group[0] += 1
        group[1] += i
        group[2] += i * 17 % 1000
    return [tuple(group) for group in result]


def exercise(args, report):
    soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
    if hard < 4096:
        raise RuntimeError('open-file hard limit must be at least 4096')
    resource.setrlimit(resource.RLIMIT_NOFILE, (max(soft, 4096), hard))
    scope = DelegatedScope(args.server_cpus, args.client_cpus)
    group, launcher = scope.server(args.memory_mib)
    report.update(cgroup=str(group), limits={name: (group / name).read_text().strip()
                  for name in ('memory.max', 'memory.swap.max', 'cpu.max', 'pids.max')})
    expected = expected_groups(args.rows)
    settings = ['max_connections=504', 'reserved_connections=0', 'superuser_reserved_connections=0',
                'connection_queue_size=300', 'connection_queue_timeout=20000', 'max_active_queries=3',
                'autovacuum=off', 'pgrust.memory_release_idle=100', 'idle_passivate_timeout=1']
    cluster = CheckedCluster(args.candidate, args.work_dir, launcher=launcher,
                             allocator='hearth', settings=settings)
    report['cluster'] = str(cluster.root)
    report['waves'] = []
    sampler = None
    try:
        with cluster:
            report['version'] = cluster.version
            assert cluster.process.pid in set(map(int, (group / 'cgroup.procs').read_text().split()))
            assert os.sched_getaffinity(cluster.process.pid) == scope.server_cpus
            with psycopg.connect(cluster.dsn, autocommit=True, application_name='churn-admin') as admin:
                admin.execute('CREATE ROLE queue_user LOGIN')
                admin.execute('CREATE ACCESS METHOD pgrcolumnar TYPE TABLE HANDLER heap_tableam_handler')
                admin.execute('CREATE TABLE churn_accounts(id integer PRIMARY KEY, balance bigint NOT NULL)')
                admin.execute('INSERT INTO churn_accounts VALUES (1,0),(2,0)')
                admin.execute('CREATE TABLE churn_facts USING pgrcolumnar AS SELECT i AS id, i%%97 AS grp, (i*17)%%1000 AS val FROM generate_series(1,%s) i', (args.rows,))
                admin.execute('GRANT SELECT, UPDATE ON churn_accounts TO queue_user')
                admin.execute('GRANT SELECT ON churn_facts TO queue_user')
                admin.execute('ANALYZE churn_accounts; ANALYZE churn_facts', prepare=False)
                report['durability'] = dict(admin.execute("SELECT name,setting FROM pg_settings WHERE name IN ('fsync','synchronous_commit','max_active_queries')").fetchall())
                assert report['durability'] == {'fsync': 'on', 'synchronous_commit': 'on', 'max_active_queries': '3'}
                admin.execute("SET statement_timeout='20s'")
                counts = [0, 0, 0]
                count_lock = threading.Lock()
                stop = threading.Event()

                def progress():
                    with count_lock:
                        return list(counts)

                def worker(index, connection):
                    completed = 0
                    while not stop.is_set():
                        if index < 2:
                            with connection.transaction():
                                value = connection.execute('UPDATE churn_accounts SET balance=balance+1 WHERE id=%s RETURNING balance', (index + 1,)).fetchone()[0]
                                assert value == completed + 1
                                assert connection.execute('SELECT balance FROM churn_accounts WHERE id=%s', (index + 1,)).fetchone()[0] == value
                        else:
                            key = completed % 97
                            actual = connection.execute('SELECT count(*),sum(id),sum(val) FROM churn_facts WHERE grp=%s', (key,)).fetchone()
                            assert actual == expected[key], (key, actual, expected[key])
                        completed += 1
                        with count_lock:
                            counts[index] = completed
                    return completed

                before = snapshot(group)
                report['resources_before'] = before
                sampler = Sampler(group)
                sampler.start()
                with ExitStack() as stack, ThreadPoolExecutor(max_workers=3) as executor:
                    connections = [stack.enter_context(psycopg.connect(cluster.dsn, user='queue_user',
                                   autocommit=True, application_name=f'churn-worker-{i}', prepare_threshold=1))
                                   for i in range(3)]
                    for connection in connections:
                        connection.execute("SET statement_timeout='10s'")
                        connection.execute('SET plan_cache_mode=force_generic_plan')
                    futures = [executor.submit(worker, i, connection) for i, connection in enumerate(connections)]
                    try:
                        deadline = time.monotonic() + 15
                        while not all(progress()):
                            for future in futures:
                                if future.done():
                                    future.result()
                                    raise RuntimeError('worker stopped during warmup')
                            if time.monotonic() > deadline:
                                raise TimeoutError('SQL worker warmup')
                            time.sleep(0.01)
                        window_start = progress()
                        report['sql_progress_windows'] = []
                        for wave in range(1, args.waves + 1):
                            for future in futures:
                                if future.done():
                                    future.result()
                                    raise RuntimeError('SQL worker stopped before churn completed')
                            result = asyncio.run(asyncio.wait_for(burst_wave(cluster, group, wave, scope.server_cpus), timeout=45))
                            deadline = time.monotonic() + 10
                            while admin.execute("SELECT count(*) FROM pg_stat_activity WHERE application_name='queue-probe'").fetchone()[0]:
                                if time.monotonic() > deadline:
                                    raise TimeoutError('closed burst backends did not drain')
                                time.sleep(0.02)
                            if wave % 10 == 0:
                                admin.execute('VACUUM (ANALYZE) churn_accounts')
                            result.update(sql_progress=progress(), after_drain=snapshot(group))
                            if wave % 5 == 0 or wave == args.waves:
                                window_end = result['sql_progress']
                                assert all(end > start for start, end in zip(window_start, window_end)), (wave, window_start, window_end)
                                report['sql_progress_windows'].append({'ending_wave': wave, 'before': window_start, 'after': window_end})
                                window_start = window_end
                            report['waves'].append(result)
                            save(args.output, report)
                            print(json.dumps({'wave': wave, 'accepted': 800, 'sql_progress': result['sql_progress'],
                                              'memory_mib': result['after_drain']['memory_current'] / 1048576}), flush=True)
                    finally:
                        stop.set()
                        report['sql_operations'] = [future.result(timeout=20) for future in futures]
                assert all(value > 0 for value in report['sql_operations'])
                expected_accounts = [(i + 1, value) for i, value in enumerate(report['sql_operations'][:2])]
                assert admin.execute('SELECT id,balance FROM churn_accounts ORDER BY id').fetchall() == expected_accounts
                report['committed_accounts'] = expected_accounts
                report['settle'] = []
                settle_start = time.monotonic()
                for target in (0, 1, 5, 15):
                    time.sleep(max(0, settle_start + target - time.monotonic()))
                    assert admin.execute('SELECT count(*) FROM churn_facts').fetchone()[0] == args.rows
                    report['settle'].append({'seconds': time.monotonic() - settle_start, 'resources': snapshot(group)})
            cluster.stop()
            cluster.start()
            with psycopg.connect(cluster.dsn, autocommit=True) as check:
                assert check.execute('SELECT id,balance FROM churn_accounts ORDER BY id').fetchall() == expected_accounts
                groups = check.execute('SELECT grp,count(*),sum(id),sum(val) FROM churn_facts GROUP BY grp ORDER BY grp').fetchall()
                assert groups == [(i, *values) for i, values in enumerate(expected)]
            report['restart_verified'] = True
        report['shutdown_exit_codes'] = cluster.shutdown_exit_codes
        assert report['shutdown_exit_codes'] == [0, 0]
        report['sampled_resources'] = sampler.stop()
        sampler = None
        after = snapshot(group)
        report.update(resources_after_shutdown=after, cpu_delta=delta(before['cpu'], after['cpu']),
                      memory_events=delta(before['events'], after['events']))
        assert not any(after['events'][key] for key in ('oom', 'oom_kill', 'oom_group_kill'))
        assert after['memory_swap_current'] == 0
        assert len(report['waves']) == args.waves and all(w['passed'] for w in report['waves'])
        report['passed'] = True
    finally:
        if sampler is not None:
            report['sampled_resources'] = sampler.stop()
        report['shutdown_exit_codes'] = cluster.shutdown_exit_codes
        report['resources_after_shutdown'] = snapshot(group)
        report['remaining_server_pids'] = (group / 'cgroup.procs').read_text().split()
        if report['remaining_server_pids']:
            report['passed'] = False
            raise RuntimeError('private server cgroup remains populated')
        group.rmdir()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--candidate', type=Path, required=True)
    parser.add_argument('--work-dir', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--memory-mib', type=int, default=4096)
    parser.add_argument('--waves', type=int, default=100)
    parser.add_argument('--rows', type=int, default=500000)
    parser.add_argument('--server-cpus', nargs='+', type=int, default=[0, 2, 4, 6])
    parser.add_argument('--client-cpus', nargs='+', type=int, default=[8, 10])
    args = parser.parse_args()
    if not __debug__ or not 1 <= args.waves <= 10000 or not 97 <= args.rows <= 10000000:
        parser.error('assertions, 1..10000 waves and 97..10000000 rows required')
    if args.output.exists():
        parser.error('output must be new')
    args.output.parent.mkdir(parents=True, exist_ok=True)
    report = {'passed': False, 'production_qualified': False, 'performance_comparison': False,
              'parameters': {'waves': args.waves, 'rows': args.rows, 'memory_mib': args.memory_mib},
              'binary_sha256': hashlib.sha256((args.candidate / 'postgres').read_bytes()).hexdigest(),
              'source_lf_sha256': {name: hashlib.sha256(Path(__file__).with_name(name).read_bytes().replace(b'\r\n', b'\n')).hexdigest()
                                   for name in ('connection_churn_probe.py', 'connection_queue_probe.py', 'cluster.py', 'resource_scope.py')}}
    save(args.output, report)
    started = time.monotonic()
    try:
        exercise(args, report)
    except BaseException as error:
        report['passed'] = False
        report['error'] = repr(error)
        raise
    finally:
        report['elapsed_seconds'] = time.monotonic() - started
        save(args.output, report)
        print(json.dumps({'passed': report['passed'], 'completed_waves': len(report.get('waves', [])), 'output': str(args.output)}), flush=True)


if __name__ == '__main__':
    main()

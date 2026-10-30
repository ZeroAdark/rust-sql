#!/usr/bin/env python3
"""Interleaved fixed-work SQL trials with separate server CPU/memory limits."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import resource
import time

import psycopg
from cluster import Cluster
from fixed_workload import PER_BUNDLE, SQL, exercise, grouped, setup, verify_state
from resource_scope import DelegatedScope, snapshot


def run_trial(args, scope, binary, memory, label, repetition):
    group, launcher = scope.server(memory)
    report = {'passed': False, 'label': label, 'repetition': repetition, 'memory_mib': memory,
              'cgroup': str(group), 'limits': {name: (group / name).read_text().strip()
                  for name in ('cpu.max', 'memory.max', 'memory.swap.max', 'pids.max')}}
    try:
        started = time.monotonic()
        with Cluster(binary, args.work_dir, launcher=launcher,
                     settings=['autovacuum=off', 'max_active_queries=4', 'work_mem=16MB']) as cluster:
            report.update(cluster=str(cluster.root), version=cluster.version)
            assert cluster.process.pid in {int(pid) for pid in (group / 'cgroup.procs').read_text().split()}
            assert os.sched_getaffinity(cluster.process.pid) == scope.server_cpus
            with psycopg.connect(cluster.dsn, autocommit=True) as connection:
                setup(connection, args.rows, args.clients)
                report['settings'] = connection.execute("SELECT name,setting,unit FROM pg_settings WHERE name IN ('fsync','synchronous_commit','max_active_queries','work_mem','shared_buffers','max_connections','connection_queue_timeout','connection_queue_size') ORDER BY name").fetchall()
                settings = {name: setting for name, setting, _ in report['settings']}
                assert settings['fsync'] == 'on' and settings['synchronous_commit'] == 'on' and settings['max_active_queries'] == '4'
                report['plans'] = {}
                for kind in ('aggregate', 'range', 'join'):
                    query = SQL[kind]
                    parameters = (1, 4096) if kind == 'range' else None
                    report['plans'][kind] = connection.execute('EXPLAIN (FORMAT JSON) ' + query, parameters).fetchone()[0]
            report['startup_setup_seconds'] = time.monotonic() - started
            report['measurement'] = exercise(cluster.dsn, group, args.rows, args.clients, args.bundles, args.warmup)
            with psycopg.connect(cluster.dsn, autocommit=True) as connection:
                report['state_before_restart'] = verify_state(connection, args.clients, args.bundles + args.warmup)
                assert connection.execute(SQL['aggregate']).fetchall() == grouped(1, args.rows, 128)
            cluster.stop()
            cluster.start()
            with psycopg.connect(cluster.dsn, autocommit=True) as connection:
                report['state_after_restart'] = verify_state(connection, args.clients, args.bundles + args.warmup)
                assert connection.execute(SQL['aggregate']).fetchall() == grouped(1, args.rows, 128)
                assert report['state_after_restart'] == report['state_before_restart']
        report['resources_after_shutdown'] = snapshot(group)
        assert not any(report['resources_after_shutdown']['events'].get(key, 0) for key in ('oom', 'oom_kill', 'oom_group_kill'))
        assert report['resources_after_shutdown']['memory_swap_current'] == 0
        report['passed'] = True
    except Exception as error:
        report['error'] = repr(error)
        report['resources_at_error'] = snapshot(group)
    finally:
        report['remaining_server_pids'] = (group / 'cgroup.procs').read_text().split()
        if not report['remaining_server_pids']:
            group.rmdir()
        else:
            report['passed'] = False
            report['cleanup_error'] = 'server cgroup remains populated; stop the private unit before another run'
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('baseline', 'candidate', 'work-dir', 'output-dir'):
        parser.add_argument('--' + name, type=Path, required=True)
    parser.add_argument('--memory-mib', type=int, nargs='+', default=[1024, 2048, 4096, 8192])
    parser.add_argument('--server-cpus', type=int, nargs='+', default=[0, 2, 4, 6])
    parser.add_argument('--client-cpus', type=int, nargs='+', default=[8, 10])
    parser.add_argument('--rows', type=int, default=1000000)
    parser.add_argument('--clients', type=int, default=6)
    parser.add_argument('--bundles', type=int, default=400)
    parser.add_argument('--warmup', type=int, default=50)
    parser.add_argument('--repeats', type=int, default=5)
    args = parser.parse_args()
    if not (4096 <= args.rows <= 10000000 and 1 <= args.clients <= 16 and 1 <= args.bundles <= 2000
            and 1 <= args.warmup <= 100 and 1 <= args.repeats <= 10):
        parser.error('rows/clients/bundles/warmup/repeats exceed bounded experiment limits')
    if len(args.memory_mib) != len(set(args.memory_mib)) or any(not 512 <= m <= 8192 for m in args.memory_mib):
        parser.error('distinct memory limits between 512 and 8192 MiB required')
    args.output_dir.mkdir(parents=True, exist_ok=False)
    resource.setrlimit(resource.RLIMIT_STACK, (65520 * 1024, resource.getrlimit(resource.RLIMIT_STACK)[1]))
    metadata = {'platform': platform.platform(), 'python': platform.python_version(),
                'server_cpus': args.server_cpus, 'client_cpus': args.client_cpus,
                'rows': args.rows, 'clients': args.clients, 'bundles': args.bundles,
                'warmup': args.warmup, 'repeats': args.repeats, 'memory_mib': args.memory_mib,
                'statements_per_bundle': PER_BUNDLE, 'binaries': {}, 'source_lf_sha256': {},
                'cpu_model': next(line.split(':', 1)[1].strip() for line in Path('/proc/cpuinfo').read_text().splitlines() if line.startswith('model name'))}
    for name in ('baseline', 'candidate'):
        binary = (getattr(args, name) / 'postgres').resolve()
        with binary.open('rb') as stream:
            metadata['binaries'][name] = {'path': str(binary), 'sha256': hashlib.file_digest(stream, 'sha256').hexdigest()}
    for name in ('resource_workload.py', 'resource_scope.py', 'fixed_workload.py', 'cluster.py'):
        path = Path(__file__).with_name(name)
        metadata['source_lf_sha256'][name] = hashlib.sha256(path.read_bytes().replace(b'\r\n', b'\n')).hexdigest()
    (args.output_dir / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
    scope = DelegatedScope(args.server_cpus, args.client_cpus)
    records = []
    for repetition in range(1, args.repeats + 1):
        # Reverse both engine and memory-limit order on alternating rounds.
        limits = args.memory_mib if repetition % 2 else args.memory_mib[::-1]
        for memory in limits:
            for label in (('baseline', 'candidate') if repetition % 2 else ('candidate', 'baseline')):
                report = run_trial(args, scope, getattr(args, label), memory, label, repetition)
                filename = f'{label}-{memory}-{repetition}.json'
                (args.output_dir / filename).write_text(json.dumps(report, indent=2) + '\n')
                item = {'file': filename, 'passed': report['passed'], 'label': label, 'memory_mib': memory,
                        'repetition': repetition, 'error': report.get('error')}
                if report.get('measurement'):
                    item.update(cpu_us_per_bundle=report['measurement']['server_cpu_us_per_bundle'],
                                bundles_per_second=report['measurement']['bundles_per_second'])
                records.append(item)
                summary = {'passed': False, 'all_finished_trials_passed': all(record['passed'] for record in records),
                           'complete': False, 'trials': records}
                (args.output_dir / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
                print(json.dumps(item), flush=True)
                if report.get('remaining_server_pids'):
                    raise RuntimeError('private server cleanup failed')
    summary['complete'] = True
    summary['passed'] = summary['all_finished_trials_passed']
    (args.output_dir / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
    return 0 if summary['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())

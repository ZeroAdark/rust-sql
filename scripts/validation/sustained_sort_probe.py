#!/usr/bin/env python3
"""Check sustained full sorts under a hard memory limit, with an optional SQL-observer control."""
import argparse
from contextlib import ExitStack
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import threading
import time

import psycopg

from connection_churn_probe import CheckedCluster
from resource_scope import DelegatedScope, Sampler, delta, snapshot
from spill_probe import copy_digest, expected, temp_files


class CpuSetScope(DelegatedScope):
    """Use hard CPU sets even when the shared pressure helper still uses CPU quotas."""
    def __init__(self, server_cpus, client_cpus):
        super().__init__(server_cpus, client_cpus)
        (self.root / 'cgroup.subtree_control').write_text('+cpuset')
        self.pin(self.driver, self.client_cpus)
        for parent in (self.root, self.root.parent, Path('/sys/fs/cgroup')):
            maximum = parent / 'cpu.max'
            if maximum.exists() and maximum.read_text().split()[0] != 'max':
                raise ValueError('CPU quotas invalidate this comparison; use a unit without CPUQuota')
        os.sched_setaffinity(0, set(range(os.cpu_count())))
        assert os.sched_getaffinity(0) == self.client_cpus

    def pin(self, group, cpus):
        (group / 'cpuset.mems').write_text((self.root / 'cpuset.mems.effective').read_text().strip())
        text = ','.join(map(str, sorted(cpus)))
        (group / 'cpuset.cpus').write_text(text)
        (group / 'cpu.max').write_text('max 100000')
        assert (group / 'cpuset.cpus.effective').read_text().strip() == text

    def server(self, memory_mib):
        group, launcher = super().server(memory_mib)
        self.pin(group, self.server_cpus)
        return group, launcher


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', type=Path, required=True)
    parser.add_argument('--engine', choices=('postgresql', 'rustsql'), required=True)
    parser.add_argument('--work-dir', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--cycles', type=int, default=12)
    parser.add_argument('--memory-mib', type=int, choices=(512, 1024, 2048, 4096, 8192), default=1024)
    parser.add_argument('--warm-reserve-mib', type=int, choices=(16, 64))
    parser.add_argument('--reuse-loader-session', action='store_true',
                        help='sort in the administrative load session, matching the historical reproducer')
    parser.add_argument('--allocator-observer', action='store_true',
                        help='diagnostic control: query allocator ownership at phase boundaries')
    args = parser.parse_args()
    if not __debug__ or args.output.exists() or not 2 <= args.cycles <= 40 or args.cycles % 2:
        parser.error('assertions, a new output and an even 2..40 cycle count are required')
    reference = args.engine == 'postgresql'
    if reference and args.warm_reserve_mib is not None:
        parser.error('the warm reserve applies only to Rust')
    if args.allocator_observer and (reference or not args.reuse_loader_session):
        parser.error('allocator diagnostics require the Rust administrative load session')
    scope = CpuSetScope([0, 2, 4, 6], [8, 10])
    parent_memory_max = (scope.root / 'memory.max').read_text().strip()
    if parent_memory_max != 'max' and int(parent_memory_max) < (args.memory_mib + 2048) * 2**20:
        parser.error('unit MemoryMax must cover the requested server limit plus the 2 GiB driver limit')
    oracle = expected(1_000_000)
    group, launcher = scope.server(args.memory_mib)
    report = {
        'passed': False, 'production_qualified': False, 'performance_qualified': False,
        'engine': args.engine, 'rows': 1_000_000, 'cycles': args.cycles, 'memory_mib': args.memory_mib,
        'allocator_sql_observer': args.allocator_observer, 'queries': [], 'samples': [], 'ownership': [],
        'reuse_loader_session': args.reuse_loader_session,
        'parent_memory_max': parent_memory_max,
        'warm_reserve_mib': args.warm_reserve_mib, 'oracle': oracle,
        'binary_sha256': hashlib.sha256((args.bin_dir / 'postgres').read_bytes()).hexdigest(),
        'source_lf_sha256': {name: hashlib.sha256(Path(__file__).with_name(name).read_bytes().replace(b'\r\n', b'\n')).hexdigest()
                             for name in ('sustained_sort_probe.py', 'spill_probe.py', 'cluster.py',
                                          'connection_churn_probe.py', 'connection_queue_probe.py', 'resource_scope.py')},
        'limits': {name: (group / name).read_text().strip()
                   for name in ('memory.max', 'memory.swap.max', 'cpu.max', 'pids.max')},
    }
    assert report['limits']['cpu.max'].split()[0] == 'max'
    proof = """import json,os
os.sched_setaffinity(0,set(range(os.cpu_count())))
assert os.sched_getaffinity(0)=={0,2,4,6}
try: os.sched_setaffinity(0,{8})
except OSError as error: assert error.errno==22
else: raise AssertionError('server escaped its CPU set')
print(json.dumps({'allowed':sorted(os.sched_getaffinity(0)),'escape_rejected':True}))
"""
    report['cpu_escape_proof'] = json.loads(subprocess.check_output(launcher + [sys.executable, '-c', proof], text=True))
    cluster = CheckedCluster(args.bin_dir, args.work_dir, reference=reference, launcher=launcher,
                             allocator=None if reference else 'hearth',
                             settings=['autovacuum=off', 'log_temp_files=0', 'log_line_prefix=[%a] '])
    for key in list(cluster.env):
        if key.startswith(('PGRUST_', 'MIMALLOC_')):
            del cluster.env[key]
    if not reference:
        cluster.env['PGRUST_ALLOCATOR'] = 'hearth'
    if args.warm_reserve_mib is not None:
        cluster.env['PGRUST_MEMPOLICY'] = f'pool_warm_zero_max={args.warm_reserve_mib * 2**20}'
    report['cluster'] = str(cluster.root)
    done = threading.Event()

    def monitor():
        try:
            while not done.is_set():
                report['samples'].append({'time': time.monotonic(), 'resources': snapshot(group)})
                done.wait(0.025)
        except Exception as error:
            report['monitor_error'] = repr(error)

    thread = threading.Thread(target=monitor)
    sampler = None
    try:
        with cluster:
            assert os.sched_getaffinity(cluster.process.pid) == scope.server_cpus
            report['version'] = cluster.version
            report['cpu_binding'] = {name: (group / name).read_text().strip()
                                     for name in ('cpuset.cpus.effective', 'cpu.max')}
            report['live_environment'] = sorted(v.decode() for v in Path('/proc', str(cluster.process.pid), 'environ').read_bytes().split(b'\0')
                                                if v.startswith((b'PGRUST_', b'MIMALLOC_')))
            with ExitStack() as sessions:
                setup = sessions.enter_context(psycopg.connect(cluster.dsn, autocommit=True))
                def observe(label):
                    if args.allocator_observer:
                        rows = setup.execute('SELECT * FROM pressure_owner_end()').fetchall()
                        report['ownership'].append({'label': label, 'time': time.monotonic(), 'rows': rows})
                if args.allocator_observer:
                    setup.execute("CREATE FUNCTION pressure_owner_end(OUT kind text, OUT source text, OUT class text, OUT events bigint, OUT committed_bytes bigint, OUT reserved_bytes bigint, OUT uses bigint, OUT account text, OUT pid int, OUT seq bigint) RETURNS SETOF record LANGUAGE internal VOLATILE AS 'pgrust_memory_owner_end'")
                    observe('startup')
                if not reference:
                    setup.execute('CREATE ACCESS METHOD pgrcolumnar TYPE TABLE HANDLER heap_tableam_handler')
                access = 'heap' if reference else 'pgrcolumnar'
                setup.execute(f'CREATE TABLE spill_facts(id integer,grp integer,val integer,payload text) USING {access}')
                setup.execute("INSERT INTO spill_facts SELECT i,i%97,(i*17)%1000,repeat(lpad(i::text,8,'0'),16) FROM generate_series(1,1000000) i")
                observe('insert')
                setup.execute('ANALYZE spill_facts')
                observe('analyze')
                setup.execute('CREATE ROLE pressure_user LOGIN; GRANT SELECT ON spill_facts TO pressure_user', prepare=False)
                report['resources_after_setup'] = snapshot(group)
                if args.reuse_loader_session:
                    connection = setup
                else:
                    setup.close()
                    connection = sessions.enter_context(psycopg.connect(cluster.dsn, user='pressure_user', autocommit=True))
                assert connection.execute('SELECT rolsuper FROM pg_roles WHERE rolname=current_user').fetchone() == (args.reuse_loader_session,)
                connection.execute("SET statement_timeout='120s'")
                report['durability'] = connection.execute("SELECT current_setting('fsync'),current_setting('synchronous_commit')").fetchone()
                assert report['durability'] == ('on', 'on')
                sampler = Sampler(group)
                sampler.start()
                thread.start()
                query = 'SELECT id,grp,val,payload FROM spill_facts ORDER BY grp DESC,id DESC'
                log = cluster.root / 'server.log'
                for index, (memory, parallel) in enumerate([('64kB', 0), ('16MB', 2)] * (args.cycles // 2)):
                    label = f'pressure-sort-{index}'
                    connection.execute("SELECT set_config('application_name',%s,false),set_config('work_mem',%s,false)", (label, memory))
                    connection.execute(f'SET max_parallel_workers_per_gather={parallel}')
                    before, offset, start = snapshot(group), log.stat().st_size, time.monotonic()
                    item = {'index': index, 'work_mem': memory, 'parallel_limit': parallel, 'passed': False, 'start': start}
                    report['queries'].append(item)
                    try:
                        actual = copy_digest(connection, query)
                        item['result'] = actual
                        assert (actual['rows'], actual['sha256']) == oracle['sort'], (actual, oracle['sort'])
                        item['passed'] = True
                    except psycopg.Error as error:
                        item.update(sqlstate=error.sqlstate, error=str(error))
                    finally:
                        item['end'] = time.monotonic()
                        item['seconds'] = item['end'] - start
                        item['resources_before'] = before
                        item['resources_after'] = snapshot(group)
                        item['cpu_delta'] = delta(before['cpu'], item['resources_after']['cpu'])
                    with log.open('rb') as handle:
                        handle.seek(offset)
                        lines = handle.read().decode(errors='replace').splitlines()
                    item['spill_files'] = [dict(path=m.group(1), bytes=int(m.group(2))) for line in lines
                                           if f'[{label}]' in line and (m := re.search(r'temporary file: path "([^"]+)", size (\d+)', line))]
                    item['remaining_temp_files'] = temp_files(cluster)
                    if item['passed']:
                        assert not item['remaining_temp_files']
                        if memory == '64kB':
                            assert sum(w['bytes'] for w in item['spill_files']) > 0, 'missing physical spill'
                    print(json.dumps({key: item.get(key) for key in ('index', 'passed', 'seconds', 'sqlstate')}), flush=True)
                    if not item['passed']:
                        break
                    observe('query-' + str(index))
                report['sampled_resources'] = sampler.stop()
                sampler = None
                done.set()
                thread.join()
                if not connection.closed:
                    report['connection_recovery'] = connection.execute('SELECT 1').fetchone()[0]
                    report['transaction_status'] = connection.info.transaction_status.name
                    assert report['connection_recovery'] == 1 and report['transaction_status'] == 'IDLE'
            # Successful and failed trials both attempt to verify acknowledged data after restart.
            try:
                cluster.stop()
            except RuntimeError as error:
                report['first_shutdown_error'] = repr(error)
            cluster.start()
            with psycopg.connect(cluster.dsn, user='pressure_user', autocommit=True) as connection:
                report['restart_count'] = connection.execute('SELECT count(*) FROM spill_facts').fetchone()[0]
                report['restart_topk'] = copy_digest(connection, query + ' LIMIT 32')
                report['restart_full_sort'] = copy_digest(connection, query)
                assert report['restart_count'] == 1_000_000
                assert (report['restart_topk']['rows'], report['restart_topk']['sha256']) == oracle['topk']
                assert (report['restart_full_sort']['rows'], report['restart_full_sort']['sha256']) == oracle['sort']
            report['passed'] = len(report['queries']) == args.cycles and all(q['passed'] for q in report['queries'])
    except Exception as error:
        report['error'] = repr(error)
    finally:
        done.set()
        if thread.ident is not None:
            thread.join()
        if sampler is not None:
            try:
                report['sampled_resources'] = sampler.stop()
            except Exception as error:
                report['monitor_error'] = repr(error)
        report['shutdown_exit_codes'] = cluster.shutdown_exit_codes
        report['resources_after_shutdown'] = snapshot(group)
        report['remaining_server_pids'] = (group / 'cgroup.procs').read_text().split()
        if (report.get('error') or report.get('monitor_error') or report['shutdown_exit_codes'] != [0, 0]
                or report['remaining_server_pids'] or report['resources_after_shutdown']['memory_swap_current']
                or any(report['resources_after_shutdown']['events'][k] for k in ('oom', 'oom_kill', 'oom_group_kill'))):
            report['passed'] = False
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps({'passed': report['passed'], 'error': report.get('error'),
                          'shutdown': report['shutdown_exit_codes'], 'events': report['resources_after_shutdown']['events']}), flush=True)
        if not report['remaining_server_pids']:
            group.rmdir()
    raise SystemExit(0 if report['passed'] else 1)


if __name__ == '__main__':
    main()

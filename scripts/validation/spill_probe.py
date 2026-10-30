#!/usr/bin/env python3
"""Verify ordered spill results, temp-file limits and cleanup in private cgroups."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import re
import time

import psycopg

from connection_churn_probe import CheckedCluster
from resource_scope import DelegatedScope, Sampler, delta, snapshot


def expected(rows):
    full, top, aggregate = hashlib.sha256(), hashlib.sha256(), hashlib.sha256()
    seen = 0
    for group in range(96, -1, -1):
        first = rows - (rows - group) % 97
        for i in range(first, 0, -97):
            row = f'{i}\t{group}\t{i * 17 % 1000}\t{str(i).zfill(8) * 16}\n'.encode()
            full.update(row)
            if seen < 32:
                top.update(row)
            seen += 1
    assert seen == rows
    for i in range(1, rows + 1):
        aggregate.update(f'{i}\t2\t{2 * (i * 17 % 1000)}\n'.encode())
    return {'sort': (rows, full.hexdigest()), 'topk': (32, top.hexdigest()),
            'aggregate': (rows, aggregate.hexdigest())}


def copy_digest(connection, sql):
    digest, rows, size = hashlib.sha256(), 0, 0
    with connection.cursor().copy('COPY (' + sql + ') TO STDOUT') as stream:
        for chunk in stream:
            digest.update(chunk)
            rows += bytes(chunk).count(b'\n')
            size += len(chunk)
    return {'rows': rows, 'bytes': size, 'sha256': digest.hexdigest()}


def temp_files(cluster):
    # Shared filesets contain names such as m35-run-... underneath pgsql_tmp.
    # Looking only for files whose own names begin with pgsql_tmp misses them.
    return sorted(str(p.relative_to(cluster.data))
                  for root in (cluster.data / 'base').rglob('pgsql_tmp') if root.is_dir()
                  for p in root.rglob('*'))


def one(args, scope, binaries, reference, oracle):
    group, launcher = scope.server(args.memory_mib)
    result = {'reference': reference, 'passed': False, 'cases': [],
              'binary_sha256': hashlib.sha256((binaries / 'postgres').read_bytes()).hexdigest(),
              'limits': {n: (group / n).read_text().strip() for n in ('memory.max', 'memory.swap.max', 'cpu.max', 'pids.max')}}
    cluster = CheckedCluster(binaries, args.work_dir, reference=reference, launcher=launcher,
                             allocator=None if reference else 'hearth',
                             settings=['autovacuum=off', 'log_temp_files=0', 'log_line_prefix=[%a] '])
    result['cluster'] = str(cluster.root)
    sampler = None
    try:
        with cluster, psycopg.connect(cluster.dsn, autocommit=True, application_name='spill-setup') as connection:
            assert cluster.process.pid in set(map(int, (group / 'cgroup.procs').read_text().split()))
            assert os.sched_getaffinity(cluster.process.pid) == scope.server_cpus
            if not reference:
                connection.execute('CREATE ACCESS METHOD pgrcolumnar TYPE TABLE HANDLER heap_tableam_handler')
            access = 'heap' if reference else 'pgrcolumnar'
            connection.execute(f'CREATE TABLE spill_facts(id integer, grp integer, val integer, payload text) USING {access}')
            connection.execute("INSERT INTO spill_facts SELECT i,i%%97,(i*17)%%1000,repeat(lpad(i::text,8,'0'),16) FROM generate_series(1,%s) i", (args.rows,))
            connection.execute('ANALYZE spill_facts')
            connection.execute("SET statement_timeout='120s'")
            result['durability'] = connection.execute("SELECT current_setting('fsync'),current_setting('synchronous_commit')").fetchone()
            assert result['durability'] == ('on', 'on')
            sampler = Sampler(group)
            sampler.start()
            queries = {'sort': 'SELECT id,grp,val,payload FROM spill_facts ORDER BY grp DESC,id DESC',
                       'topk': 'SELECT id,grp,val,payload FROM spill_facts ORDER BY grp DESC,id DESC LIMIT 32',
                       'aggregate': 'SELECT id,count(*),sum(val) FROM spill_facts CROSS JOIN generate_series(1,2) n GROUP BY id ORDER BY id'}
            cases = [('sort', '64kB', 0), ('sort', '16MB', 2), ('aggregate', '64kB', 0),
                     ('aggregate', '64kB', 2), ('topk', '64kB', 0), ('topk', '64kB', 2)]
            log = cluster.root / 'server.log'
            for index, (kind, memory, parallel) in enumerate(cases):
                label = f'spill-{index}-{kind}'
                connection.execute("SELECT set_config('application_name',%s,false)", (label,))
                connection.execute("SELECT set_config('work_mem',%s,false)", (memory,))
                connection.execute(f'SET max_parallel_workers_per_gather={parallel}')
                offset, before, started = log.stat().st_size, snapshot(group), time.monotonic()
                try:
                    actual = copy_digest(connection, queries[kind])
                except psycopg.Error as error:
                    result['failed_case'] = {'index': index, 'kind': kind, 'work_mem': memory,
                                             'parallel_limit': parallel, 'sqlstate': error.sqlstate,
                                             'resources': snapshot(group), 'temp_files': temp_files(cluster),
                                             'transaction_status': connection.info.transaction_status.name}
                    try:
                        recovery = {'row_count': connection.execute('SELECT count(*) FROM spill_facts').fetchone()[0],
                                    'topk': copy_digest(connection, queries['topk'])}
                        recovery['passed'] = (recovery['row_count'] == args.rows and
                                              recovery['topk']['sha256'] == oracle['topk'][1] and
                                              not temp_files(cluster))
                        result['failed_case']['recovery'] = recovery
                    except Exception as recovery_error:
                        result['failed_case']['recovery'] = {'passed': False, 'error': repr(recovery_error)}
                    raise
                assert (actual['rows'], actual['sha256']) == oracle[kind], (kind, actual, oracle[kind])
                with log.open('rb') as stream:
                    stream.seek(offset)
                    lines = stream.read().decode(errors='replace').splitlines()
                witnesses = [dict(path=m.group(1), bytes=int(m.group(2))) for line in lines
                             if f'[{label}]' in line and (m := re.search(r'temporary file: path "([^"]+)", size (\d+)', line))]
                require_spill = kind != 'topk' and memory == '64kB'
                if require_spill:
                    assert sum(w['bytes'] for w in witnesses) > 0, (label, 'no physical spill witness')
                remaining = temp_files(cluster)
                assert not remaining, remaining
                after = snapshot(group)
                result['cases'].append({'kind': kind, 'work_mem': memory, 'parallel_limit': parallel,
                                        'result': actual, 'temp_files': witnesses, 'required_spill': require_spill, 'remaining_temp_files': remaining,
                                        'resources_before': before, 'resources_after': after,
                                        'seconds': time.monotonic() - started, 'cpu_delta': delta(before['cpu'], after['cpu']), 'passed': True})
                print(json.dumps({'reference': reference, 'case': label, 'passed': True,
                                  'spill_bytes': sum(w['bytes'] for w in witnesses)}), flush=True)
            result['temp_limit_errors'] = []
            for parallel in (0, 2):
                connection.execute("SET work_mem='64kB'; SET temp_file_limit=0", prepare=False)
                connection.execute(f'SET max_parallel_workers_per_gather={parallel}')
                try:
                    copy_digest(connection, queries['sort'])
                except psycopg.Error as error:
                    assert error.sqlstate == '53400', (error.sqlstate, str(error))
                    result['temp_limit_errors'].append({'parallel_limit': parallel, 'sqlstate': error.sqlstate})
                else:
                    raise AssertionError('temp_file_limit=0 did not stop a spilling sort')
                assert connection.info.transaction_status.name == 'IDLE'
                assert connection.execute('SELECT count(*) FROM spill_facts').fetchone()[0] == args.rows
                assert not temp_files(cluster)
                connection.execute('SET temp_file_limit=-1')
                assert copy_digest(connection, queries['topk'])['sha256'] == oracle['topk'][1]
            result['cancellations'] = []
            for parallel in (0, 2):
                connection.execute("SET work_mem='64kB'; SET temp_file_limit=-1", prepare=False)
                connection.execute(f'SET max_parallel_workers_per_gather={parallel}')
                assert not temp_files(cluster)
                with ThreadPoolExecutor(max_workers=1) as executor:
                    future = executor.submit(copy_digest, connection, queries['sort'])
                    deadline = time.monotonic() + 10
                    witnessed = []
                    while not future.done() and time.monotonic() < deadline:
                        for relative in temp_files(cluster):
                            try:
                                path = cluster.data / relative
                                if path.is_file() and path.stat().st_size > 0:
                                    witnessed.append(relative)
                            except FileNotFoundError:
                                pass  # A completed run may be removed during the scan.
                        if witnessed:
                            break
                        time.sleep(0.001)
                    connection.cancel()
                    try:
                        future.result(timeout=15)
                    except psycopg.Error as error:
                        assert error.sqlstate == '57014', (error.sqlstate, str(error))
                        assert witnessed, 'cancellation did not witness a nonempty live spill file'
                        result['cancellations'].append({'parallel_limit': parallel, 'sqlstate': error.sqlstate,
                                                       'live_spill_files': witnessed})
                    else:
                        raise AssertionError('spilling sort finished before cancellation')
                assert connection.info.transaction_status.name == 'IDLE'
                assert not temp_files(cluster), temp_files(cluster)
                assert copy_digest(connection, queries['topk'])['sha256'] == oracle['topk'][1]
            result['passed'] = True
    except BaseException as error:
        result['error'] = repr(error)
        result['passed'] = False
    finally:
        if sampler is not None:
            result['sampled_resources'] = sampler.stop()
        result['shutdown_exit_codes'] = cluster.shutdown_exit_codes
        result['resources_after_shutdown'] = snapshot(group)
        result['remaining_server_pids'] = (group / 'cgroup.procs').read_text().split()
        if result['shutdown_exit_codes'] != [0] or result['remaining_server_pids']:
            result['passed'] = False
        if any(result['resources_after_shutdown']['events'][k] for k in ('oom', 'oom_kill', 'oom_group_kill')):
            result['passed'] = False
        if result['resources_after_shutdown']['memory_swap_current']:
            result['passed'] = False
        if not result['remaining_server_pids']:
            group.rmdir()
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--candidate', type=Path, required=True)
    parser.add_argument('--reference', type=Path, required=True)
    parser.add_argument('--work-dir', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--rows', type=int, default=500000)
    parser.add_argument('--memory-mib', type=int, default=1024)
    parser.add_argument('--server-cpus', nargs='+', type=int, default=[0, 2, 4, 6])
    parser.add_argument('--client-cpus', nargs='+', type=int, default=[8, 10])
    args = parser.parse_args()
    if not __debug__ or not 100000 <= args.rows <= 2000000 or args.output.exists():
        parser.error('assertions, 100000..2000000 rows and a new output path required')
    scope = DelegatedScope(args.server_cpus, args.client_cpus)
    oracle = expected(args.rows)
    report = {'passed': False, 'production_qualified': False, 'performance_comparison': False,
              'rows': args.rows, 'memory_mib': args.memory_mib, 'oracle': oracle, 'servers': {},
              'source_lf_sha256': {n: hashlib.sha256(Path(__file__).with_name(n).read_bytes().replace(b'\r\n',b'\n')).hexdigest()
                                   for n in ('spill_probe.py','connection_churn_probe.py','connection_queue_probe.py','cluster.py','resource_scope.py')}}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    for name, binary, reference in [('reference', args.reference, True), ('candidate', args.candidate, False)]:
        report['servers'][name] = one(args, scope, binary, reference, oracle)
        args.output.write_text(json.dumps(report, indent=2) + '\n')
    report['passed'] = all(r['passed'] for r in report['servers'].values())
    args.output.write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps({'passed': report['passed'], 'servers': {name: {'passed': r['passed'], 'error': r.get('error')} for name,r in report['servers'].items()}}))
    if not report['passed']:
        raise SystemExit(1)


if __name__ == '__main__':
    main()

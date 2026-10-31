#!/usr/bin/env python3
"""Check allocator residency reporting against the host VMA naming capability.

Runs owned clusters under a delegated cgroup. This is correctness/diagnostic
evidence, not a performance comparison. Optional retained binaries reproduce the
old incomplete counter without treating it as an accurate memory measurement.
"""
import argparse
import ctypes
import errno
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import resource

import psycopg

from connection_churn_probe import CheckedCluster, save
from fixed_workload import setup, exercise, verify_state
from resource_scope import DelegatedScope, snapshot


def naming_capability():
    libc = ctypes.CDLL(None, use_errno=True)
    libc.mmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int,
                          ctypes.c_int, ctypes.c_int, ctypes.c_long]
    libc.mmap.restype = ctypes.c_void_p
    libc.munmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t]
    page = os.sysconf('SC_PAGE_SIZE')
    address = libc.mmap(None, page, 3, 0x22, -1, 0)
    if address == ctypes.c_void_p(-1).value:
        raise OSError(ctypes.get_errno(), 'mmap capability probe')
    try:
        ctypes.c_ubyte.from_address(address).value = 1
        rc = libc.prctl(ctypes.c_int(0x53564d41), ctypes.c_ulong(0),
                        ctypes.c_ulong(address), ctypes.c_ulong(page),
                        ctypes.c_char_p(b'rustsql-residency-probe'))
        error = ctypes.get_errno() if rc != 0 else 0
        maps = Path('/proc/self/maps').read_text()
        named = any('[anon:rustsql-residency-probe]' in line for line in maps.splitlines())
        assert (rc == 0 and named) or (rc == -1 and error == errno.EINVAL and not named), (rc, error, named)
        return {'kernel': platform.release(), 'supported': rc == 0,
                'prctl_return': rc, 'errno': error, 'visible_in_maps': named}
    finally:
        assert libc.munmap(address, page) == 0


def run_case(args, scope, report, label, binaries, allocator, definitions):
    group, launcher = scope.server(2048)
    cluster = CheckedCluster(binaries, args.work_dir, launcher=launcher, allocator=allocator,
                             settings=['autovacuum=off', 'max_active_queries=6', 'work_mem=16MB',
                                       'shared_buffers=128MB', 'max_parallel_workers_per_gather=2'])
    for key in list(cluster.env):
        if key.startswith('MIMALLOC_'):
            del cluster.env[key]
    result = {'label': label, 'allocator': allocator, 'passed': False,
              'cluster': str(cluster.root), 'stages': [],
              'binary_sha256': hashlib.sha256((binaries/'postgres').read_bytes()).hexdigest()}
    report['cases'].append(result)
    try:
        with cluster:
            result['environment'] = sorted(v.decode() for v in Path('/proc', str(cluster.process.pid), 'environ').read_bytes().split(b'\0')
                                           if v.startswith((b'PGRUST_', b'MIMALLOC_')))
            assert result['environment'] == ['PGRUST_ALLOCATOR='+allocator]
            with psycopg.connect(cluster.dsn, autocommit=True) as connection:
                for definition in definitions:
                    connection.execute(definition)

                def capture(stage):
                    assert connection.execute('SHOW pgrust.allocator').fetchone()[0] == allocator
                    rows = connection.execute('SELECT account,account_bytes,class FROM pgrust_memory_accounts() ORDER BY account').fetchall()
                    values = {name: value for name, value, _ in rows}
                    owner = connection.execute("SELECT class,committed_bytes,reserved_bytes FROM pgrust_memory_owner_end() WHERE class='mimalloc_resident'").fetchall()
                    assert values['resident'] > 0 and values['resident_cgroup'] > 0
                    keys = {'allocator_resident', 'allocator_retained', 'allocator_live'}
                    if report['kernel_probe']['supported']:
                        assert keys <= values.keys() and values['allocator_resident'] > 0
                    elif label == 'candidate':
                        assert keys.isdisjoint(values), 'an unavailable total must be omitted'
                        assert not owner, 'the incomplete mimalloc term must be omitted too'
                    else:
                        assert keys <= values.keys(), 'retained binary reproduces the old counter'
                        if allocator == 'mimalloc':
                            assert values['allocator_resident'] == 0
                    result['stages'].append({'stage': stage, 'accounts': rows, 'mimalloc_owner_rows': owner,
                                             'resources': snapshot(group)})

                capture('startup')
                setup(connection, 1000000, 6)
                capture('setup')
                result['workload'] = exercise(cluster.dsn, group, 1000000, 6, 30, 10, bundle_rate=9.4)
                assert result['workload']['passed']
                capture('after_workload')
                result['before_restart'] = verify_state(connection, 6, 40)
            cluster.stop()
            cluster.start()
            with psycopg.connect(cluster.dsn, autocommit=True) as connection:
                result['after_restart'] = verify_state(connection, 6, 40)
                assert result['after_restart'] == result['before_restart']
                capture('after_restart')
        result['passed'] = True
    except Exception as error:
        result['error'] = repr(error)
        raise
    finally:
        result['shutdown_exit_codes'] = cluster.shutdown_exit_codes
        result['after_shutdown'] = snapshot(group)
        result['remaining_pids'] = (group/'cgroup.procs').read_text().split()
        save(args.output/'report.json', report)
        assert not result['remaining_pids']
        group.rmdir()
    assert result['shutdown_exit_codes'] == [0, 0]
    assert all(result['after_shutdown']['events'][key] == 0 for key in ['oom', 'oom_kill', 'max'])
    assert result['after_shutdown']['memory_swap_current'] == 0
    print(json.dumps({'label': label, 'allocator': allocator, 'passed': True}), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', type=Path, required=True)
    parser.add_argument('--baseline-bin', type=Path)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--work-dir', type=Path, required=True)
    parser.add_argument('--server-cpus', type=int, nargs='+', default=[0, 2, 4, 6])
    parser.add_argument('--client-cpus', type=int, nargs='+', default=[8, 10])
    args = parser.parse_args()
    if not __debug__:
        parser.error('validation requires Python assertions enabled')
    args.output.mkdir(parents=True, exist_ok=False)
    resource.setrlimit(resource.RLIMIT_STACK, (65520*1024, resource.getrlimit(resource.RLIMIT_STACK)[1]))
    scope = DelegatedScope(args.server_cpus, args.client_cpus)
    sql_path = Path(__file__).resolve().parents[1]/'memory-broker-views.sql'
    sql = sql_path.read_text()
    definitions = [re.search(r'CREATE OR REPLACE FUNCTION '+name+r'\(.*?;', sql, re.S).group()
                   for name in ['pgrust_memory_accounts', 'pgrust_memory_owner_end']]
    report = {'passed': False, 'purpose': 'counter correctness; not performance acceptance',
              'kernel_probe': naming_capability(), 'cases': [], 'source_sha256': {}}
    for name in [Path(__file__).name, 'cluster.py', 'connection_churn_probe.py', 'fixed_workload.py', 'resource_scope.py']:
        path = Path(__file__).with_name(name)
        report['source_sha256'][name] = hashlib.sha256(path.read_bytes()).hexdigest()
    report['source_sha256'][sql_path.name] = hashlib.sha256(sql_path.read_bytes()).hexdigest()
    try:
        for label, binaries in [('baseline', args.baseline_bin), ('candidate', args.bin_dir)]:
            if binaries is not None:
                for allocator in ['hearth', 'mimalloc']:
                    run_case(args, scope, report, label, binaries, allocator, definitions)
        report['passed'] = True
    finally:
        save(args.output/'report.json', report)
    return 0


if __name__ == '__main__':
    raise SystemExit(main())

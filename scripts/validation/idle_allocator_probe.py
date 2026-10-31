#!/usr/bin/env python3
"""Check live session state across the allocator's existing idle timer stages."""
import argparse
from contextlib import ExitStack
import hashlib
import json
from pathlib import Path
import time

import psycopg

from cluster import Cluster


def run(args, report):
    settings = ['autovacuum=off', 'pgrust.memory_release_idle=50',
                'pgrust.memory_release_threshold=1', 'idle_passivate_timeout=1',
                'log_min_messages=debug1', 'log_line_prefix=[%a] ']
    with Cluster(args.candidate, args.work_dir, allocator='hearth', settings=settings) as cluster:
        report.update(cluster=str(cluster.root), version=cluster.version)
        with psycopg.connect(cluster.dsn, autocommit=True) as admin:
            admin.execute('CREATE ROLE idle_probe LOGIN')
            admin.execute('CREATE TABLE idle_items AS SELECT i AS id, repeat(i::text,24) AS payload FROM generate_series(1,72) i')
            admin.execute('GRANT SELECT ON idle_items TO idle_probe')
            report['settings'] = admin.execute("SELECT name,setting FROM pg_settings WHERE name IN ('pgrust.memory_release_idle','pgrust.memory_release_threshold','idle_passivate_timeout') ORDER BY name").fetchall()
            assert dict(report['settings']) == {'pgrust.memory_release_idle': '50',
                                               'pgrust.memory_release_threshold': '1',
                                               'idle_passivate_timeout': '1'}
            report['allocator'] = admin.execute('SHOW pgrust.allocator').fetchone()[0]
            assert report['allocator'] == 'hearth'
        expected = [(i, str(i) * 24) for i in range(1, 73)]
        with ExitStack() as stack:
            sessions = []
            for i in range(args.clients):
                name, mode = f'idle-probe-{i}', ('prepared', 'transaction', 'hold')[i % 3]
                connection = stack.enter_context(psycopg.connect(
                    cluster.dsn, user='idle_probe', application_name=name, autocommit=True))
                connection.execute('SET plan_cache_mode=force_generic_plan')
                connection.execute('PREPARE retained_lookup(int) AS SELECT id,payload FROM idle_items WHERE id=$1')
                assert connection.execute('EXECUTE retained_lookup(7)').fetchall() == [expected[6]]
                if mode != 'prepared':
                    connection.execute('BEGIN')
                    hold = ' WITH HOLD' if mode == 'hold' else ''
                    connection.execute('DECLARE retained_cursor SCROLL CURSOR' + hold + ' FOR SELECT id,payload FROM idle_items ORDER BY id')
                    assert connection.execute('FETCH 3 FROM retained_cursor').fetchall() == expected[:3]
                    if mode == 'hold':
                        connection.execute('COMMIT')
                sessions.append((name, mode, connection))

            # Ignore timer events from setup. Rearm after every retained object exists,
            # then leave the sessions untouched until both eligible stages have run.
            setup_lines = (cluster.root / 'server.log').read_text(errors='replace').splitlines()
            baseline = {name: {
                'release': sum(f'[{name}]' in line and 'idle memory release:' in line for line in setup_lines),
                'passivate': sum(f'[{name}]' in line and 'idle passivation:' in line for line in setup_lines),
            } for name, _, _ in sessions}
            report['timer_baseline'] = baseline
            for _, _, connection in sessions:
                assert connection.execute('SELECT 1').fetchone() == (1,)
            deadline = time.monotonic() + 8
            witnesses = {}
            while time.monotonic() < deadline:
                lines = (cluster.root / 'server.log').read_text(errors='replace').splitlines()
                witnesses = {name: {
                    'release': sum(f'[{name}]' in line and 'idle memory release:' in line for line in lines) - baseline[name]['release'],
                    'passivate': sum(f'[{name}]' in line and 'idle passivation:' in line for line in lines) - baseline[name]['passivate'],
                } for name, _, _ in sessions}
                if all(witnesses[name]['release'] and (mode == 'transaction' or witnesses[name]['passivate'])
                       for name, mode, _ in sessions):
                    break
                time.sleep(0.02)
            report['timer_witnesses'] = witnesses
            for name, mode, connection in sessions:
                witness = witnesses[name]
                assert witness['release'] > 0, (name, 'idle release did not fire')
                assert (witness['passivate'] > 0) == (mode != 'transaction'), (name, 'passivation stage')
                assert connection.info.transaction_status.name == ('INTRANS' if mode == 'transaction' else 'IDLE')
                assert connection.execute('EXECUTE retained_lookup(7)').fetchall() == [expected[6]]
                if mode != 'prepared':
                    assert connection.execute('FETCH ALL FROM retained_cursor').fetchall() == expected[3:]
                    assert connection.execute('FETCH FIRST FROM retained_cursor').fetchall() == expected[:1]
                    connection.execute('CLOSE retained_cursor')
                    if mode == 'transaction':
                        connection.execute('COMMIT')
                assert connection.execute('EXECUTE retained_lookup(72)').fetchall() == expected[-1:]
                report['checks'].append({'name': name, 'mode': mode, 'passed': True})
        assert cluster.process.poll() is None
    report['passed'] = True


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--candidate', type=Path, required=True)
    parser.add_argument('--work-dir', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--clients', type=int, default=12)
    args = parser.parse_args()
    if not 3 <= args.clients <= 60:
        parser.error('--clients must be between 3 and 60')
    report = {'passed': False, 'checks': [], 'clients': args.clients,
              'binary_sha256': hashlib.sha256((args.candidate / 'postgres').read_bytes()).hexdigest(),
              'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              'production_qualified': False}
    try:
        run(args, report)
    except Exception as exc:
        report['error'] = repr(exc)
        raise
    finally:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps({'passed': report['passed'], 'checks': len(report['checks']), 'output': str(args.output)}))


if __name__ == '__main__':
    main()

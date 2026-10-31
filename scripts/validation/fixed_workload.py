"""Fixed mixed work, independent value oracles and exact bounded latency samples.

Each bundle has twelve point reads, four acknowledged updates and three analytics
queries. No throughput-dependent change in query mix enters CPU/bundle results.
Oracle cycles are bounded by the data generator's period, independent of row count.
"""
from array import array
from collections import Counter
import hashlib
import json
import math
import multiprocessing
import time

from resource_scope import Sampler, delta, snapshot


SQL = {
    'read': 'SELECT id,balance FROM bench_accounts WHERE id=%s',
    'update': 'UPDATE bench_accounts SET balance=balance+1 WHERE id=%s RETURNING id,balance',
    'aggregate': 'SELECT g,count(*),sum(v) FROM bench_facts GROUP BY g ORDER BY g',
    'range': 'SELECT count(*),sum(v) FROM bench_facts WHERE id BETWEEN %s AND %s',
    'join': 'SELECT d.region,count(*),sum(f.v) FROM bench_facts f JOIN bench_dim d ON d.g=f.g GROUP BY d.region ORDER BY d.region',
}
PER_BUNDLE = {'read': 12, 'update': 4, 'aggregate': 1, 'range': 1, 'join': 1}


def progression_sum(first, count, stride):
    period = 1000 // math.gcd(17 * stride, 1000)
    values = [(17 * (first + i * stride)) % 1000 for i in range(min(count, period))]
    cycles, tail = divmod(count, period)
    return cycles * sum(values) + sum(values[:tail])


def grouped(lo, hi, groups):
    result = []
    for group in range(groups):
        first = lo + (group - lo) % groups
        count = max(0, (hi - first) // groups + 1)
        if count:
            result.append((group, count, progression_sum(first, count, groups)))
    return result


def expected_updates(clients, bundles):
    result = Counter()
    for client in range(clients):
        for bundle in range(bundles):
            for phase in range(4):
                result[client * 20000 + (bundle * 67 + phase * 19) % 20000 + 1] += 1
    return sorted(result.items())


def latency_summary(values):
    ordered = sorted(values)
    if not ordered:
        raise ValueError('empty latency sample')
    return {'count': len(ordered), **{f'p{p}_ms': ordered[math.ceil(len(ordered) * p / 100) - 1] / 1e6
                                    for p in (50, 95, 99)}, 'max_ms': ordered[-1] / 1e6}


def setup(connection, rows, clients, *, reference=False):
    connection.execute('CREATE TABLE bench_accounts(id integer PRIMARY KEY,balance bigint NOT NULL)')
    connection.execute('INSERT INTO bench_accounts SELECT i,0 FROM generate_series(1,%s) g(i)', (clients * 20000,))
    if not reference:
        connection.execute('CREATE ACCESS METHOD pgrcolumnar TYPE TABLE HANDLER heap_tableam_handler')
    access = 'heap' if reference else 'pgrcolumnar'
    connection.execute(f'CREATE TABLE bench_facts(id integer,g integer,v integer) USING {access}')
    connection.execute('INSERT INTO bench_facts SELECT i,i%%128,(i*17)%%1000 FROM generate_series(1,%s) g(i)', (rows,))
    connection.execute('CREATE TABLE bench_dim(g integer PRIMARY KEY,region integer)')
    connection.execute('INSERT INTO bench_dim SELECT i,i%8 FROM generate_series(0,127) g(i)')
    for table in ('bench_accounts', 'bench_facts', 'bench_dim'):
        connection.execute(f'VACUUM ANALYZE {table}')
    connection.execute('CREATE ROLE bench_user LOGIN')
    connection.execute('GRANT SELECT,UPDATE ON bench_accounts TO bench_user')
    connection.execute('GRANT SELECT ON bench_facts,bench_dim TO bench_user')
    connection.execute('CHECKPOINT')


def exercise(dsn, group, rows, clients, bundles, warmup, *, bundle_rate=None):
    import psycopg
    from psycopg.conninfo import make_conninfo
    oracle = {'aggregate': grouped(1, rows, 128), 'join': grouped(1, rows, 8)}
    ranges = []
    for i in range(64):
        lo = 1 + (i * 7919) % (rows - 4096 + 1)
        ranges.append((lo, lo + 4095, [(4096, progression_sum(lo, 4096, 1))]))
    context = multiprocessing.get_context('fork')
    ready, complete = context.Queue(), context.Queue()
    begin, release = context.Event(), context.Event()
    app_dsn = make_conninfo(dsn, user='bench_user')

    def worker(client):
        sent_ready = False
        try:
            with psycopg.connect(app_dsn, autocommit=True) as connection:
                connection.execute("SET statement_timeout='60s'; SET lock_timeout='10s'")
                assert connection.execute('SELECT rolsuper FROM pg_roles WHERE rolname=current_user').fetchone() == (False,)
                balances = Counter()
                timings = {kind: array('Q') for kind in (*SQL, 'bundle')}
                types = {}

                def query(kind, parameters, expected, measure):
                    start = time.perf_counter_ns()
                    cursor = connection.execute(SQL[kind], parameters, prepare=True)
                    actual = cursor.fetchall()
                    elapsed = time.perf_counter_ns() - start
                    assert actual == expected, (kind, expected[:2], actual[:2])
                    observed = [column.type_code for column in cursor.description]
                    expected_types = [23, 20] if kind in ('read', 'update') else [20, 20] if kind == 'range' else [23, 20, 20]
                    assert observed == expected_types, (kind, observed)
                    types[kind] = observed
                    if measure:
                        timings[kind].append(elapsed)

                def bundle(number, measure):
                    started = time.perf_counter_ns()
                    for phase in range(4):
                        for read in range(3):
                            key = client * 20000 + (number * 67 + phase * 19 + read * 7) % 20000 + 1
                            query('read', (key,), [(key, balances[key])], measure)
                        key = client * 20000 + (number * 67 + phase * 19) % 20000 + 1
                        query('update', (key,), [(key, balances[key] + 1)], measure)
                        balances[key] += 1
                    query('aggregate', None, oracle['aggregate'], measure)
                    lo, hi, expected = ranges[(number + client * 7) % len(ranges)]
                    query('range', (lo, hi), expected, measure)
                    query('join', None, oracle['join'], measure)
                    if measure:
                        timings['bundle'].append(time.perf_counter_ns() - started)

                for number in range(warmup):
                    bundle(number, False)
                ready.put({'client': client, 'passed': True, 'types': types})
                sent_ready = True
                if not begin.wait(180):
                    raise TimeoutError('measurement start not received')
                started = time.monotonic_ns()
                schedule_lag = array('Q')
                interval_ns = round(clients * 1e9 / bundle_rate) if bundle_rate else 0
                for number in range(warmup, warmup + bundles):
                    if interval_ns:
                        due = started + (number - warmup) * interval_ns + client * interval_ns // clients
                        time.sleep(max(0, (due - time.monotonic_ns()) / 1e9))
                        schedule_lag.append(max(0, time.monotonic_ns() - due))
                    bundle(number, True)
                ended = time.monotonic_ns()
                complete.put({'client': client, 'passed': True, 'start_ns': started, 'end_ns': ended, 'timings': timings, 'schedule_lag': schedule_lag})
                if not release.wait(180):
                    raise TimeoutError('measurement release not received')
        except Exception as error:
            (complete if sent_ready else ready).put({'client': client, 'passed': False, 'error': repr(error)})

    workers = [context.Process(target=worker, args=(i,)) for i in range(clients)]
    sampler = Sampler(group)
    sampled = None
    try:
        # Fork before starting the sampler. Connections remain open until all
        # cgroup counters have been read, including PostgreSQL process workers.
        for process in workers:
            process.start()
        warm = [ready.get(timeout=180) for _ in workers]
        if not all(item['passed'] for item in warm):
            raise AssertionError(warm)
        before = snapshot(group)
        client_before = snapshot(group.parent / 'driver')
        sampler.start()
        started = time.monotonic_ns()
        begin.set()
        results = [complete.get(timeout=600) for _ in workers]
        after = snapshot(group)
        client_after = snapshot(group.parent / 'driver')
        sampled = sampler.stop()
        if not all(item['passed'] for item in results):
            raise AssertionError(results)
        elapsed = (max(item['end_ns'] for item in results) - started) / 1e9
    finally:
        release.set()
        if sampler.thread.ident is not None and sampler.thread.is_alive():
            sampler.stop()
        for process in workers:
            if process.pid is not None:
                process.join(timeout=5)
                if process.is_alive():
                    process.terminate()
                    process.join(timeout=5)
    assert all(process.exitcode == 0 for process in workers), 'client process failed'
    measured = {}
    for kind in (*SQL, 'bundle'):
        samples = [value for result in results for value in result['timings'][kind]]
        count = clients * bundles * (PER_BUNDLE.get(kind, 1))
        assert len(samples) == count
        measured[kind] = latency_summary(samples)
    cpu = delta(before['cpu'], after['cpu'])
    events = delta(before['events'], after['events'])
    assert not any(events.get(name, 0) for name in ('oom', 'oom_kill', 'oom_group_kill'))
    assert after['memory_swap_current'] == 0
    return {'passed': True, 'clients': clients, 'bundles_per_client': bundles, 'warmup_per_client': warmup,
            'fact_rows': rows, 'elapsed_seconds': elapsed, 'bundles_per_second': clients * bundles / elapsed,
            'server_cpu_seconds': cpu['usage_usec'] / 1e6,
            'server_cpu_us_per_bundle': cpu['usage_usec'] / (clients * bundles),
            'offered_bundles_per_second': bundle_rate,
            'schedule_lag': latency_summary([v for r in results for v in r['schedule_lag']]) if bundle_rate else None,
            'client_cpu_delta': delta(client_before['cpu'], client_after['cpu']),
            'cpu_delta': cpu, 'memory_event_delta': events, 'resource_before': before, 'resource_after': after,
            'sampled_resources': sampled, 'latency': measured, 'warmup': warm}


def verify_state(connection, clients, bundles):
    expected = expected_updates(clients, bundles)
    actual = connection.execute('SELECT id,balance FROM bench_accounts WHERE balance<>0 ORDER BY id').fetchall()
    assert actual == expected
    assert sum(value for _, value in actual) == clients * bundles * 4
    return {'changed_accounts': len(actual), 'committed_updates': clients * bundles * 4,
            'values_sha256': hashlib.sha256(json.dumps(actual).encode()).hexdigest()}

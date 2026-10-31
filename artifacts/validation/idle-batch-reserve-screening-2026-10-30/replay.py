"""Check the rejected idle-batch prototype and the separate 500 ms policy screen."""
from pathlib import Path, PurePosixPath
import hashlib
import io
import json
import re
import statistics
import tarfile

PG = '07b16e1aa4f0e2206c37b339b6e4cc813aa8ab9e3c5516334f753b3d90d0c536'
RETAINED = '8e5bfc2b093055a5d2257dc817037a2b2f2906faf7c1b83e431f472a2f0513f0'
CANDIDATE = 'd33e65908c8882633f74d8462fc554567d87b04ca7bff6f7bf03299129f2e05c'


def summarize(trials, engines):
    getters = {
        'cpu_ms_per_bundle': lambda m: m['server_cpu_us_per_bundle'] / 1000,
        'sql_per_second': lambda m: m['bundles_per_second'] * 19,
        'charged_peak_mib': lambda m: m['sampled_resources']['sampled_memory_current_peak_bytes'] / 2**20,
        'pss_peak_mib': lambda m: m['sampled_resources']['sampled_summed_pss_peak_bytes'] / 2**20,
    }
    for kind in ('read', 'update', 'aggregate', 'range', 'join', 'bundle'):
        getters[kind + '_p99_ms'] = lambda m, k=kind: m['latency'][k]['p99_ms']
    result = {'phase': 'medium', 'trials': len(trials), 'engines': {}}
    for engine in engines:
        arm = [d for d in trials if d['engine'] == engine]
        result['engines'][engine] = {name: statistics.median(fn(d['measurement']) for d in arm) for name, fn in getters.items()}
    controls = {'candidate_change_percent': 'rustsql-baseline'}
    if 'rustsql-prior-policy' in engines:
        controls['prior_policy_change_percent'] = 'rustsql-prior-policy'
    for name, control in controls.items():
        result[name] = {k: 100 * (result['engines']['rustsql'][k] / v - 1) for k, v in result['engines'][control].items()}
    result['preservation_failures'] = [k for k, v in result['candidate_change_percent'].items()
                                       if (k == 'cpu_ms_per_bundle' or k.endswith('_p99_ms')) and v > 5]
    return result


def cpu_window(trials, engines, start):
    values = {engine: [] for engine in engines}
    throttled = []
    for d in sorted(trials, key=lambda d: (d['engine'], d['repetition'])):
        before, after = d[start]['cpu'], d['resources_after_workload']['cpu']
        delta = {k: after[k] - before[k] for k in ('usage_usec', 'nr_throttled', 'throttled_usec')}
        assert delta['usage_usec'] > 0 and all(v >= 0 for v in delta.values())
        values[d['engine']].append(delta['usage_usec'])
        if delta['nr_throttled'] or delta['throttled_usec']:
            throttled.append({'engine': d['engine'], 'repetition': d['repetition'], **delta})
    candidate = statistics.median(values['rustsql'])
    return {
        'cpu_us_by_engine': values, 'throttled_trials': throttled,
        'candidate_change_percent': {e: 100 * (candidate / statistics.median(values[e]) - 1)
                                     for e in engines if e != 'rustsql'},
        'rust_comparisons_valid': not any(d['engine'] != 'postgresql' for d in throttled),
    }


def review(data):
    read = lambda name: json.loads(data[name])
    result = {}
    for label, prototype in [('idle-batch-reserve-medium-20261030', True), ('aged-warm-500-medium-20261030', False)]:
        meta = read(label + '/metadata.json')
        engines = ['postgresql', 'rustsql-baseline'] + (['rustsql-prior-policy'] if prototype else []) + ['rustsql']
        policy = 'pool_warm_zero_max=16777216,pool_decay_ms=' + ('200' if prototype else '500')
        identities = {e: PG if e == 'postgresql' else CANDIDATE if prototype and e == 'rustsql' else RETAINED for e in engines}
        assert meta['binary_sha256'] == identities and meta['candidate_policy'] == policy
        assert {k: meta[k] for k in ('phase', 'rows', 'clients', 'memory_mib', 'bundles', 'warmup', 'repeats', 'bundle_rate')} == {
            'phase': 'medium', 'rows': 1000000, 'clients': 6, 'memory_mib': 2048,
            'bundles': 30, 'warmup': 10, 'repeats': 5, 'bundle_rate': 9.4,
        }
        assert meta['server_cpus'] == [0, 2, 4, 6] and meta['client_cpus'] == [8, 10]
        assert meta['statements_per_bundle'] == {'read': 12, 'update': 4, 'aggregate': 1, 'range': 1, 'join': 1}
        for name, sha in meta['source_lf_sha256'].items():
            assert hashlib.sha256(data[label + '-source/' + name].replace(b'\r\n', b'\n')).hexdigest() == sha
        summary = read(label + '/summary.json')
        assert summary['complete'] and summary['passed'] and len(summary['trials']) == 5 * len(engines)
        expected = [(e, i) for i in range(1, 6) for e in (engines if i % 2 else engines[::-1])]
        assert [(d['engine'], d['repetition']) for d in summary['trials']] == expected
        trials = []
        for engine, i in expected:
            d = read(f'{label}/medium-{engine}-{i}.json')
            m = d['measurement']
            assert d['engine'] == engine and d['repetition'] == i and d['phase'] == 'medium'
            assert d['passed'] and m['passed'] and d['state_before_restart'] == d['state_after_restart']
            assert d['shutdown_exit_codes'] == [0, 0] and not d['remaining_server_pids']
            chosen = policy if engine in ('rustsql', 'rustsql-prior-policy') else ''
            assert d['requested_policy'] == chosen and d['live_policy_env'] == (['PGRUST_MEMPOLICY=' + chosen] if chosen else [])
            assert d['limits'] == {'cpu.max': '400000 100000', 'memory.max': '2147483648', 'memory.swap.max': '0', 'pids.max': '1024'}
            for key in ('cpu_delta', 'client_cpu_delta'):
                assert m[key]['nr_throttled'] == 0 and m[key]['throttled_usec'] == 0
            for key, value in m['cpu_delta'].items():
                assert value == m['resource_after']['cpu'].get(key, 0) - m['resource_before']['cpu'].get(key, 0)
            assert not any(m['memory_event_delta'].values())
            for key in ('resources_before_setup', 'resources_before_workload', 'resources_after_workload', 'resources_after_shutdown'):
                assert d[key]['memory_swap_current'] == 0
                assert not any(d[key]['events'][k] for k in ('max', 'oom', 'oom_kill', 'oom_group_kill'))
            settings = {name: value for name, value, _ in d['settings']}
            assert settings['fsync'] == settings['synchronous_commit'] == 'on'
            assert settings['max_parallel_workers_per_gather'] == '2'
            assert settings['shared_buffers'] == settings['work_mem'] == '16384'
            assert d['state_after_restart']['committed_updates'] == 960
            for kind, n in (meta['statements_per_bundle'] | {'bundle': 1}).items():
                assert m['latency'][kind]['count'] == 180 * n
            trials.append(d)
        timed = summarize(trials, engines)
        assert timed == read(label + ('/four-engine-review.json' if prototype else '/three-engine-review.json'))
        assert timed['preservation_failures'] == (['read_p99_ms', 'aggregate_p99_ms', 'bundle_p99_ms'] if prototype else [])
        result[label] = {'timed': timed,
                         'warmup_and_measurement_cpu': cpu_window(trials, engines, 'resources_before_workload'),
                         'setup_warmup_and_measurement_cpu': cpu_window(trials, engines, 'resources_before_setup')}
    return result


def verify(root):
    if not __debug__:
        raise SystemExit('assertions required')
    manifest = json.loads((root / 'manifest.json').read_text())
    blob = (root / 'evidence.tar.gz').read_bytes()
    assert hashlib.sha256(blob).hexdigest() == manifest['archive_sha256']
    data = {}
    with tarfile.open(fileobj=io.BytesIO(blob), mode='r:gz') as archive:
        for entry in archive:
            name = PurePosixPath(entry.name)
            assert entry.isfile() and not name.is_absolute() and '..' not in name.parts and entry.name not in data
            data[entry.name] = archive.extractfile(entry).read()
    assert set(data) == set(manifest['members'])
    for name, content in data.items():
        assert hashlib.sha256(content).hexdigest() == manifest['members'][name], name
    assert review(data) == json.loads((root / 'results.json').read_text())
    read = lambda name: json.loads(data[name])
    candidate = read('candidate/manifest.json')
    assert candidate['binaries']['postgres'] == CANDIDATE and len(candidate['runtime_source_sha256']) == 3
    assert hashlib.sha256(data['candidate/candidate.patch']).hexdigest() == candidate['runtime_patch_sha256']
    for name, sha in candidate['runtime_source_sha256'].items():
        assert hashlib.sha256(data['candidate/source/' + name]).hexdigest() == sha
    counts = re.findall(r'test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;', data['candidate/idle-batch-reserve-tests.log'].decode())
    assert len(counts) == 85 and tuple(sum(int(x[i]) for x in counts) for i in range(3)) == (117, 0, 1)
    negative = read('candidate/idle-batch-reserve-negative-control.json')
    assert negative['passed'] and negative['exit'] == 101 and negative['positive_restored']
    assert negative['positive_source_sha256'] == candidate['runtime_source_sha256']['crates/_support/pagepool/src/central.rs']
    assert negative['expected_failure'] in data['candidate/idle-batch-reserve-negative-control.log'].decode()
    assert b'Finished `release` profile' in data['candidate/idle-batch-reserve-build.log']
    protocol = read('candidate/idle-batch-reserve-protocol.json')
    assert protocol['passed'] and set(protocol['cases']) == {'default', 'fast'}
    for path, sha in protocol['source_sha256'].items():
        name = Path(path).name
        location = 'candidate/' if name == 'run_idle_batch_protocol.py' else 'support/'
        assert hashlib.sha256(data[location + name]).hexdigest() == sha
    for name, case in protocol['cases'].items():
        policy = '' if name == 'default' else 'pool_warm_zero_max=16777216,pool_decay_ms=200'
        assert case['policy'] == policy and case['report']['passed']
        assert case['witnesses'] == [{'reference': True, 'live_policy_env': []},
                                    {'reference': False, 'live_policy_env': ['PGRUST_MEMPOLICY=' + policy] if policy else []}]
        engines = case['report']['engines']
        assert engines['rustsql']['binary_sha256'] == CANDIDATE and engines['postgresql']['binary_sha256'] == PG
        assert engines['postgresql']['result'] == engines['rustsql']['result']
        assert len(engines['rustsql']['result']['checks']) == 8 and engines['rustsql']['result']['committed_updates'] == 192
        assert all(d['shutdown_exit_codes'] == [0] for d in engines.values())
    assert read('candidate/decision.json')['accepted'] is False
    restoration = read('candidate/restoration.json')
    assert restoration['source_head'] == candidate['source_head']
    assert set(restoration['source_files_restored']) == set(candidate['runtime_source_sha256'])
    assert restoration['source_hashes_verified_before_restore'] and restoration['target_binary_copied_from_retained_not_rebuilt']
    assert restoration['retained_binary_sha256'] == RETAINED
    return {'passed': True, 'hashed_members': len(data), 'mixed_trials': 35, 'timed_statements': 119700,
            'allocator_tests': 117, 'protocol_settings': 2, 'production_qualified': False}


if __name__ == '__main__':
    print(json.dumps(verify(Path(__file__).resolve().parent)))

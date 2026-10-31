"""Verify frozen build-profile experiments; never extract or execute archived contents."""
from pathlib import Path, PurePosixPath
import hashlib
import io
import json
import statistics
import tarfile

ENGINES = ('postgresql', 'rustsql-baseline', 'rustsql')
BINARIES = {
    'postgresql': '07b16e1aa4f0e2206c37b339b6e4cc813aa8ab9e3c5516334f753b3d90d0c536',
    'rustsql-baseline': '8e5bfc2b093055a5d2257dc817037a2b2f2906faf7c1b83e431f472a2f0513f0',
    'rustsql': 'f0cf1dbe3f5df578e7e09835040f1b2abc777fd5fa8959c34e12047ba4e86c3f',
}
CAMPAIGNS = {'dist-profile-medium-20261030': (None, 'medium', 30, 9.4), 'dist-profile-capacity-20261030': (None, 'capacity', 50, None)}
REJECTED = {}


def summarize(trials, phase):
    getters = {
        'cpu_ms_per_bundle': lambda m: m['server_cpu_us_per_bundle'] / 1000,
        'sql_per_second': lambda m: m['bundles_per_second'] * 19,
        'charged_peak_mib': lambda m: m['sampled_resources']['sampled_memory_current_peak_bytes'] / 2**20,
        'pss_peak_mib': lambda m: m['sampled_resources']['sampled_summed_pss_peak_bytes'] / 2**20,
    }
    for kind in ('read', 'update', 'aggregate', 'range', 'join', 'bundle'):
        getters[kind + '_p99_ms'] = lambda m, k=kind: m['latency'][k]['p99_ms']
    result = {'phase': phase, 'trials': len(trials), 'engines': {}}
    for engine in ENGINES:
        arm = [d for d in trials if d['engine'] == engine]
        assert len(arm) == 5
        result['engines'][engine] = {
            name: statistics.median(fn(d['measurement']) for d in arm)
            for name, fn in getters.items()
        }
    result['candidate_change_percent'] = {
        name: 100 * (result['engines']['rustsql'][name] / value - 1)
        for name, value in result['engines']['rustsql-baseline'].items()
    }
    result['preservation_failures'] = [
        name for name, value in result['candidate_change_percent'].items()
        if (name == 'sql_per_second' and phase == 'capacity' and value < -5)
        or ((name == 'cpu_ms_per_bundle' or name.endswith('_p99_ms')) and value > 5)
    ]
    return result


def cpu_window(trials, start):
    """Include warmup/cleanup; record rather than conceal any throttled arm."""
    cpu = {engine: [] for engine in ENGINES}
    throttled = []
    for d in sorted(trials, key=lambda d: (d['engine'], d['repetition'])):
        before = d[start]['cpu']
        after = d['resources_after_workload']['cpu']
        delta = {key: after[key] - before[key] for key in ('usage_usec', 'nr_throttled', 'throttled_usec')}
        assert delta['usage_usec'] > 0 and all(v >= 0 for v in delta.values())
        cpu[d['engine']].append(delta['usage_usec'])
        if delta['nr_throttled'] or delta['throttled_usec']:
            throttled.append({'engine': d['engine'], 'repetition': d['repetition'],
                              'nr_throttled': delta['nr_throttled'], 'throttled_usec': delta['throttled_usec']})
    baseline = cpu['rustsql-baseline']
    candidate = cpu['rustsql']
    return {
        'baseline_cpu_us': baseline, 'candidate_cpu_us': candidate,
        'median_change_pct': 100 * (statistics.median(candidate) / statistics.median(baseline) - 1),
        'paired_change_pct': [100 * (c / b - 1) for b, c in zip(baseline, candidate)],
        'throttled_trials': throttled,
        'rust_comparison_valid': not any(d['engine'] != 'postgresql' for d in throttled),
    }


def review(data, allow_recorded_throttle=False):
    read = lambda name: json.loads(data[name])
    results = {}
    for label, (decay, phase, bundles, rate) in CAMPAIGNS.items():
        meta = read(label + '/metadata.json')
        summary = read(label + '/summary.json')
        policy = ''
        assert meta['candidate_policy'] == policy
        assert meta['binary_sha256'] == BINARIES
        assert {k: meta[k] for k in ('phase', 'rows', 'clients', 'memory_mib', 'bundles', 'warmup', 'repeats', 'bundle_rate')} == {
            'phase': phase, 'rows': 1000000, 'clients': 6, 'memory_mib': 2048,
            'bundles': bundles, 'warmup': 10, 'repeats': 5, 'bundle_rate': rate,
        }
        assert meta['server_cpus'] == [0, 2, 4, 6] and meta['client_cpus'] == [8, 10]
        assert meta['statements_per_bundle'] == {'read': 12, 'update': 4, 'aggregate': 1, 'range': 1, 'join': 1}
        assert summary['complete'] and summary['passed'] and len(summary['trials']) == 15
        expected_order = [{'engine': e, 'repetition': r, 'passed': True} for r in range(1, 6)
                          for e in (ENGINES if r % 2 else tuple(reversed(ENGINES)))]
        assert summary['trials'] == expected_order
        for name, sha in meta['source_lf_sha256'].items():
            assert hashlib.sha256(data[label + '-source/' + name].replace(b'\r\n', b'\n')).hexdigest() == sha
        trials = []
        throttled = {}
        for engine in ENGINES:
            for repetition in range(1, 6):
                d = read(f'{label}/{phase}-{engine}-{repetition}.json')
                m = d['measurement']
                assert d['engine'] == engine and d['repetition'] == repetition and d['phase'] == phase
                assert d['passed'] and m['passed'] and d['shutdown_exit_codes'] == [0, 0]
                assert not d['remaining_server_pids'] and d['state_before_restart'] == d['state_after_restart']
                assert d['requested_policy'] == (policy if engine == 'rustsql' else '')
                assert d['live_policy_env'] == (['PGRUST_MEMPOLICY=' + policy] if engine == 'rustsql' and policy else [])
                assert d['limits'] == {'cpu.max': '400000 100000', 'memory.max': '2147483648', 'memory.swap.max': '0', 'pids.max': '1024'}
                assert not any(m['memory_event_delta'].values())
                assert m['client_cpu_delta']['nr_throttled'] == 0 and m['client_cpu_delta']['throttled_usec'] == 0
                cpu = m['cpu_delta']
                for key, value in cpu.items():
                    assert value == m['resource_after']['cpu'].get(key, 0) - m['resource_before']['cpu'].get(key, 0)
                if cpu['nr_throttled'] or cpu['throttled_usec']:
                    assert allow_recorded_throttle, 'timed server CPU throttling'
                    throttled[f'{phase}-{engine}-{repetition}.json'] = (cpu['nr_throttled'], cpu['throttled_usec'])
                for key in ('resources_before_setup', 'resources_before_workload', 'resources_after_workload', 'resources_after_shutdown'):
                    assert not any(d[key]['events'][k] for k in ('oom', 'oom_kill', 'oom_group_kill', 'max'))
                    assert d[key]['memory_swap_current'] == 0
                settings = {name: value for name, value, _ in d['settings']}
                assert settings['fsync'] == settings['synchronous_commit'] == 'on'
                assert settings['max_parallel_workers_per_gather'] == '2'
                assert settings['shared_buffers'] == settings['work_mem'] == '16384'
                assert d['state_after_restart']['committed_updates'] == 6 * (bundles + 10) * 4
                for kind, count in (meta['statements_per_bundle'] | {'bundle': 1}).items():
                    assert m['latency'][kind]['count'] == 6 * bundles * count
                trials.append(d)
        timed = summarize(trials, phase)
        assert throttled == REJECTED.get(label, {})
        if throttled:
            rejection = read(label + '/performance-validation.json')
            assert rejection['evidence_valid'] is False
            assert {d['report']: (d['cpu_delta']['nr_throttled'], d['cpu_delta']['throttled_usec'])
                    for d in rejection['invalid_trials']} == throttled
        else:
            assert timed == read(label + '/three-engine-review.json')
        results[label] = {
            'timed_performance_valid': not throttled,
            'invalid_timed_trials': {key: list(value) for key, value in throttled.items()},
            'timed': timed,
            'warmup_and_measurement_cpu': cpu_window(trials, 'resources_before_workload'),
            'setup_warmup_and_measurement_cpu': cpu_window(trials, 'resources_before_setup'),
        }
    return results


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
    results = review(data)
    assert results == json.loads((root / 'results.json').read_text())
    assert results['dist-profile-medium-20261030']['timed']['preservation_failures'] == []
    assert results['dist-profile-capacity-20261030']['timed']['preservation_failures'] == ['update_p99_ms', 'aggregate_p99_ms']
    read = lambda name: json.loads(data[name])
    candidate = read('candidate/manifest.json')
    assert candidate['profile'] == 'dist' and candidate['runtime_defaults_changed'] is False and candidate['qualified'] is False
    assert candidate['binaries']['postgres'] == BINARIES['rustsql']
    assert candidate['retained_sha256'] == BINARIES['rustsql-baseline']
    assert hashlib.sha256(data['candidate/retained-source-difference.patch']).hexdigest() == candidate['source_difference_sha256']
    build = read('candidate/metadata.json')
    assert build['exit'] == 0 and build['elapsed_seconds'] > 0 and build['source_head'] == candidate['source_head']
    assert build['command'] == ['cargo', 'build', '--locked', '--profile', 'dist', '-j', '2', '-p', 'main_main', '-p', 'initdb']
    assert b'Finished `dist` profile' in data['candidate/build.log']
    before = data['source-proof/rowfilter-before.rs'].replace(b'\r\n', b'\n')
    after = data['source-proof/rowfilter-after.rs'].replace(b'\r\n', b'\n')
    marker = b'#[cfg(test)]\nmod tests {'
    assert before.count(marker) == after.count(marker) == 1 and before.split(marker)[0] == after.split(marker)[0]
    retained = read('source-proof/retained-manifest.json')
    assert retained['binaries']['postgres'] == BINARIES['rustsql-baseline']
    assert hashlib.sha256(data['source-proof/postgres.rs']).hexdigest() == retained['runtime_source_sha256']['crates/backend/main/main_main/src/bin/postgres.rs']
    witness = read('source-proof/retained-runtime.json')
    assert witness['target_release_sha256'] == witness['retained_sha256'] == BINARIES['rustsql-baseline']
    assert witness['runtime_diff'] == '' and witness['defaults_changed'] is False
    protocol = read('support/dist-profile-candidate-correctness.json')
    assert protocol['passed']
    engines = protocol['engines']
    assert engines['postgresql']['binary_sha256'] == BINARIES['postgresql'] and engines['rustsql']['binary_sha256'] == BINARIES['rustsql']
    assert engines['postgresql']['result'] == engines['rustsql']['result']
    assert len(engines['rustsql']['result']['checks']) == 8 and engines['rustsql']['result']['committed_updates'] == 192
    assert all(d['shutdown_exit_codes'] == [0] for d in engines.values())
    assert read('candidate/decision.json')['status'] == 'not_selected'
    return {'passed': True, 'hashed_members': len(data), 'mixed_trials': 30,
            'timed_statements': 136800, 'protocol_groups': 8, 'production_qualified': False}

if __name__ == '__main__':
    print(json.dumps(verify(Path(__file__).resolve().parent)))

"""Verify recorded trials; never run archived code or promote a runtime."""
from pathlib import Path
import argparse
import hashlib
import json
import statistics

if not __debug__:
    raise RuntimeError('Assertions must remain enabled')

BINARIES = {
    'postgresql': '07b16e1aa4f0e2206c37b339b6e4cc813aa8ab9e3c5516334f753b3d90d0c536',
    'rustsql-baseline': '0be60915b40c9f70f39dcb3a0fd1f8918608a20095f767ec081a5b5320caa277',
    'rustsql': 'f7f033774c2c37157eb633045ff6bbc6f1956a96b370bcbae6e03e211abca220',
}
PHASES = {'medium': ('medium', 2048, 30), 'capacity': ('capacity', 2048, 1500),
          'medium-1g': ('medium', 1024, 30)}


def review_phase(read, key):
    phase, memory, bundles = PHASES[key]
    label = 'known-budget-' + key + '-20261031'
    engines = ('postgresql', 'rustsql-baseline', 'rustsql') if phase == 'medium' else ('rustsql-baseline', 'rustsql')

    def get(name):
        return json.loads(read(name))

    meta = get(label + '/metadata.json')
    for field, expected in {'phase': phase, 'memory_mib': memory, 'rows': 1_000_000,
                            'clients': 6, 'bundles': bundles, 'warmup': 10, 'repeats': 5,
                            'server_cpus': [0, 2, 4, 6], 'client_cpus': [8, 10],
                            'statements_per_bundle': {'read': 12, 'update': 4, 'aggregate': 1, 'range': 1, 'join': 1},
                            'binary_sha256': {engine: BINARIES[engine] for engine in engines}}.items():
        assert meta[field] == expected, (key, field, meta[field])
    assert meta['bundle_rate'] == (9.4 if phase == 'medium' else None)
    for name, sha in meta['source_lf_sha256'].items():
        assert hashlib.sha256(read(label + '-source/' + name).replace(b'\r\n', b'\n')).hexdigest() == sha
    preflight = get(label + '-cpuset-preflight.json')
    assert preflight['exit'] == 0 and json.loads(preflight['stdout'])['passed']
    assert get(label + '-command.json')['exit'] == 0
    expected_trials = [{'engine': engine, 'repetition': rep, 'passed': True}
                       for rep in range(1, 6) for engine in (engines if rep % 2 else engines[::-1])]
    summary = get(label + '/summary.json')
    assert summary['complete'] and summary['passed'] and summary['trials'] == expected_trials
    trials = []
    for item in expected_trials:
        engine, rep = item['engine'], item['repetition']
        d = get(f'{label}/{phase}-{engine}-{rep}.json')
        m = d['measurement']
        assert (d['phase'], d['engine'], d['repetition']) == (phase, engine, rep)
        assert d['passed'] and m['passed'] and not d.get('error')
        assert d['shutdown_exit_codes'] == [0, 0] and not d['remaining_server_pids']
        assert d['state_before_restart'] == d['state_after_restart']
        assert d['state_after_restart']['committed_updates'] == 6 * (bundles + 10) * 4
        assert d['live_environment'] == ([] if engine == 'postgresql' else ['PGRUST_ALLOCATOR=hearth'])
        if engine != 'postgresql':
            assert d['allocator_before'] == d['allocator_after'] == 'hearth'
        assert d['cpu_binding'] == {'cpuset.cpus': '0,2,4,6', 'cpuset.cpus.effective': '0,2,4,6',
                                    'cpu.max': 'max 100000', 'parent_cpu_max': 'max 100000'}
        assert d['limits'] == {'cpu.max': 'max 100000', 'memory.max': str(memory * 2**20),
                               'memory.swap.max': '0', 'pids.max': '1024'}
        for side in ('cpu_delta', 'client_cpu_delta'):
            assert m[side]['nr_throttled'] == m[side]['throttled_usec'] == 0
        assert not any(m['memory_event_delta'].values())
        for endpoint in ('resources_before_setup', 'resources_before_workload', 'resources_after_workload', 'resources_after_shutdown'):
            assert d[endpoint]['memory_swap_current'] == 0
            assert all(d[endpoint]['events'][event] == 0 for event in ('max', 'oom', 'oom_kill', 'oom_group_kill'))
        settings = {name: value for name, value, _ in d['settings']}
        assert settings['fsync'] == settings['synchronous_commit'] == 'on'
        assert settings['max_parallel_workers_per_gather'] == '2'
        assert settings['shared_buffers'] == settings['work_mem'] == '16384'
        assert m['elapsed_seconds'] >= (15 if phase == 'medium' else 20)
        assert m['clients'] == 6 and m['bundles_per_client'] == bundles and m['warmup_per_client'] == 10
        assert m['fact_rows'] == 1_000_000
        assert sorted(w['client'] for w in m['warmup']) == list(range(6))
        for warmup in m['warmup']:
            assert warmup['passed'] and warmup['types'] == {
                'read': [23, 20], 'update': [23, 20], 'aggregate': [23, 20, 20],
                'range': [20, 20], 'join': [23, 20, 20]}
        assert m['server_cpu_us_per_bundle'] == m['cpu_delta']['usage_usec'] / (6 * bundles)
        for kind, count in (meta['statements_per_bundle'] | {'bundle': 1}).items():
            assert m['latency'][kind]['count'] == 6 * bundles * count
        trials.append(d)
    getters = {
        'cpu_ms_per_bundle': lambda m: m['server_cpu_us_per_bundle'] / 1000,
        'sql_per_second': lambda m: m['bundles_per_second'] * 19,
        'charged_peak_mib': lambda m: m['sampled_resources']['sampled_memory_current_peak_bytes'] / 2**20,
        'pss_peak_mib': lambda m: m['sampled_resources']['sampled_summed_pss_peak_bytes'] / 2**20,
    }
    for kind in ('read', 'update', 'aggregate', 'range', 'join', 'bundle'):
        getters[kind + '_p99_ms'] = lambda m, k=kind: m['latency'][k]['p99_ms']
    values = {engine: {metric: statistics.median(fn(t['measurement']) for t in trials if t['engine'] == engine)
                       for metric, fn in getters.items()} for engine in engines}
    changes = {metric: 100 * (values['rustsql'][metric] / val - 1)
               for metric, val in values['rustsql-baseline'].items()}
    failures = [metric for metric, change in changes.items()
                if (metric == 'sql_per_second' and phase == 'capacity' and change < -5)
                or ((metric == 'cpu_ms_per_bundle' or metric.endswith('_p99_ms')) and change > 5)]
    result = {'phase': phase, 'engines': values, 'candidate_change_percent': changes,
              'preservation_failures': failures, 'evidence_valid': True, 'passed': not failures}
    assert result == get(label + '/candidate-review.json'), 'stored review differs from raw evidence'
    windows = {}
    for name, start, end in [
        ('warmup_and_measurement', 'resources_before_workload', 'resources_after_workload'),
        ('setup_warmup_and_measurement', 'resources_before_setup', 'resources_after_workload'),
        ('setup_through_shutdown_and_restart', 'resources_before_setup', 'resources_after_shutdown')]:
        by_engine = {}
        for engine in engines:
            values_cpu = []
            for d in trials:
                if d['engine'] != engine:
                    continue
                for endpoint in (start, end):
                    assert d[endpoint]['cpu']['nr_throttled'] == d[endpoint]['cpu']['throttled_usec'] == 0
                elapsed_cpu = (d[end]['cpu']['usage_usec'] - d[start]['cpu']['usage_usec']) / 1e6
                assert elapsed_cpu > 0
                values_cpu.append(elapsed_cpu)
            by_engine[engine] = statistics.median(values_cpu)
        windows[name] = {'median_server_cpu_seconds': by_engine,
                         'candidate_change_percent': 100 * (by_engine['rustsql'] / by_engine['rustsql-baseline'] - 1)}
    return {'memory_mib': memory, 'trial_count': len(trials), 'timed_statements': len(trials) * 6 * bundles * 19,
            'warmup_statements': len(trials) * 6 * 10 * 19, 'cpu_windows': windows, **result}


def tamper_controls(read):
    cases = [
        ('binary', 'known-budget-medium-20261031/metadata.json', lambda d: d['binary_sha256'].__setitem__('rustsql', '0' * 64)),
        ('cpu_binding', 'known-budget-medium-20261031/medium-rustsql-1.json', lambda d: d['cpu_binding'].__setitem__('cpuset.cpus.effective', '0-15')),
        ('row_count', 'known-budget-medium-20261031/medium-rustsql-1.json', lambda d: d['measurement'].__setitem__('fact_rows', 999_999)),
        ('restart', 'known-budget-medium-20261031/medium-rustsql-1.json', lambda d: d['state_after_restart'].__setitem__('committed_updates', 0)),
    ]
    rejected = []
    for name, path, alter in cases:
        data = json.loads(read(path))
        alter(data)
        def modified(p):
            return json.dumps(data).encode() if p == path else read(p)
        try:
            review_phase(modified, 'medium')
        except AssertionError:
            rejected.append(name)
        else:
            raise AssertionError('forged evidence accepted: ' + name)
    return rejected


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--base', type=Path, default=Path('/home/rustsql'))
    parser.add_argument('--phases', nargs='+', choices=tuple(PHASES), default=list(PHASES))
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    read = lambda p: (args.base / p).read_bytes()
    phases = {key: review_phase(read, key) for key in args.phases}
    report = {'evidence_valid': True, 'recorded_evidence_only': True, 'candidate_promoted': False,
              'all_requested_preservation_guards_pass': all(p['passed'] for p in phases.values()),
              'phases': phases, 'timed_statements': sum(p['timed_statements'] for p in phases.values()),
              'warmup_statements': sum(p['warmup_statements'] for p in phases.values()),
              'tamper_controls_rejected': tamper_controls(read),
              'screen_reporting_correction': 'Original screen reused its phase key for a resource loop, overwriting top-level phase summaries. This review reconstructs every requested phase from unchanged raw trials and checks each original per-phase review.'}
    if args.output:
        assert not args.output.exists(), 'preserve previous review'
        args.output.write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps(report))


if __name__ == '__main__':
    main()

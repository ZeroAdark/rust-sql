"""Verify frozen policy experiments; never extract or execute archived contents."""
from pathlib import Path, PurePosixPath
import hashlib
import io
import json
import statistics
import tarfile

if not __debug__:
    raise RuntimeError('Verification requires enabled assertions.')

ENGINES = ('rustsql-baseline', 'rustsql')
BINARIES = {
    'rustsql-baseline': '57baa2ef25bdfebccd3bf851fb84692828a72e2b8541faddfe1ace0a281cabe6',
    'rustsql': '0be60915b40c9f70f39dcb3a0fd1f8918608a20095f767ec081a5b5320caa277',
}
CAMPAIGNS = {}
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
        assert meta['cpu_binding'] == 'private kernel CPU sets: four server CPUs and two driver CPUs; no CFS quota'
        preflight = read(label + '-cpuset-preflight.json')
        assert preflight['exit'] == 0
        command = preflight['command']
        assert '--property=AllowedCPUs=0,2,4,6,8,10' in command
        assert '--property=Delegate=cpu memory pids cpuset' in command
        assert command[-1] == '/home/rustsql/' + label + '-source/cpuset_preflight.py'
        proof = json.loads(preflight['stdout'])
        assert proof['passed'] and proof['driver_allowed'] == [8,10]
        assert proof['server'] == {'allowed':[0,2,4,6], 'escape_rejected':True}
        assert proof['server_cpu_max'] == proof['parent_cpu_max'] == 'max 100000'
        assert proof['server_resources']['cpu']['nr_throttled'] == 0
        assert proof['server_resources']['cpu']['throttled_usec'] == 0
        policy = ''
        assert meta['candidate_policy'] == policy
        assert meta['acceptance_guard_percent'] == 5
        assert meta['purpose'].startswith('Predeclared five-pair 1500-bundle capacity check of the memory-derived prefault ceiling')
        assert meta['binary_sha256'] == BINARIES
        assert {k: meta[k] for k in ('phase', 'rows', 'clients', 'memory_mib', 'bundles', 'warmup', 'repeats', 'bundle_rate')} == {
            'phase': phase, 'rows': 1000000, 'clients': 6, 'memory_mib': 2048,
            'bundles': bundles, 'warmup': 10, 'repeats': 5, 'bundle_rate': rate,
        }
        assert meta['server_cpus'] == [0, 2, 4, 6] and meta['client_cpus'] == [8, 10]
        assert meta['statements_per_bundle'] == {'read': 12, 'update': 4, 'aggregate': 1, 'range': 1, 'join': 1}
        assert summary['complete'] and summary['passed'] and len(summary['trials']) == 10
        assert summary['trials'] == [{'engine': e, 'repetition': r, 'passed': True} for r in range(1,6) for e in (ENGINES if r%2 else tuple(reversed(ENGINES)))]
        for name, sha in meta['source_lf_sha256'].items():
            assert hashlib.sha256(data[label + '-source/' + name].replace(b'\r\n', b'\n')).hexdigest() == sha
        trials = []
        throttled = {}
        for engine in ENGINES:
            for repetition in range(1, 6):
                d = read(f'{label}/{phase}-{engine}-{repetition}.json')
                m = d['measurement']
                assert d['engine'] == engine and d['repetition'] == repetition and d['phase'] == phase
                assert m['elapsed_seconds'] >= 20
                assert d['passed'] and m['passed'] and d['shutdown_exit_codes'] == [0, 0]
                assert not d['remaining_server_pids'] and d['state_before_restart'] == d['state_after_restart']
                assert d['live_environment'] == ([] if engine == 'postgresql' else ['PGRUST_ALLOCATOR=hearth'])
                if engine != 'postgresql':
                    assert d['allocator_before'] == d['allocator_after'] == 'hearth'
                assert d['cpu_binding'] == {'cpuset.cpus':'0,2,4,6','cpuset.cpus.effective':'0,2,4,6','cpu.max':'max 100000','parent_cpu_max':'max 100000'}
                assert d['limits'] == {'cpu.max': 'max 100000', 'memory.max': '2147483648', 'memory.swap.max': '0', 'pids.max': '1024'}
                assert not any(m['memory_event_delta'].values())
                assert m['client_cpu_delta']['nr_throttled'] == 0 and m['client_cpu_delta']['throttled_usec'] == 0
                cpu = m['cpu_delta']
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


if __name__ == '__main__':
    import sys
    label, phase = sys.argv[1:3]
    bundles, rate = (30, 9.4) if phase == 'medium' else (1500, None)
    CAMPAIGNS[label] = (None, phase, bundles, rate)
    base = Path('/home/rustsql'); data = {}
    for folder in (base / label, base / (label + '-source')):
        for p in folder.iterdir():
            if p.is_file(): data[p.relative_to(base).as_posix()] = p.read_bytes()
    data[label + '-cpuset-preflight.json'] = (base / (label + '-cpuset-preflight.json')).read_bytes()
    trials = [json.loads(data[f'{label}/{phase}-{e}-{r}.json']) for e in ENGINES for r in range(1,6)]
    timed = summarize(trials, phase)
    path = base/label/'three-engine-review.json'
    path.write_text(json.dumps(timed, indent=2)+'\n')
    data[f'{label}/three-engine-review.json'] = path.read_bytes()
    results = review(data)
    (base / label / 'supplementary-review.json').write_text(json.dumps(results, indent=2) + '\n')
    r = results[label]
    print(json.dumps({'valid': True, 'preservation_failures': r['timed']['preservation_failures'],
                     'warmup_cpu_change_pct':r['warmup_and_measurement_cpu']['median_change_pct'],
                     'setup_cpu_change_pct':r['setup_warmup_and_measurement_cpu']['median_change_pct'],
                     'warmup_rust_valid':r['warmup_and_measurement_cpu']['rust_comparison_valid'],
                     'setup_rust_valid':r['setup_warmup_and_measurement_cpu']['rust_comparison_valid']}))

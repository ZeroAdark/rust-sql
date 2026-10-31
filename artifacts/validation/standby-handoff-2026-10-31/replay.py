"""Verify frozen policy experiments; never extract or execute archived contents."""
from pathlib import Path, PurePosixPath
import hashlib
import io
import json
import statistics
import tarfile

ENGINES = ('postgresql', 'rustsql-baseline', 'rustsql')
BINARIES = {
    'postgresql': '07b16e1aa4f0e2206c37b339b6e4cc813aa8ab9e3c5516334f753b3d90d0c536',
    'rustsql-baseline': '02cfbca6869bb352c8081b1fb275cdd4317b33a72b8feb05c563408e4c48d454',
    'rustsql': '96370df0fa407cbfbf9d27d018c8bc534a38554add8a9d675e62aa49f8ce7d5c',
}
CAMPAIGNS = {'standby-medium-20261030': (None, 'medium', 30, 9.4)}
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
                assert d['passed'] and m['passed'] and d['shutdown_exit_codes'] == [0, 0]
                assert not d['remaining_server_pids'] and d['state_before_restart'] == d['state_after_restart']
                assert d['live_environment'] == ([] if engine == 'postgresql' else ['PGRUST_ALLOCATOR=hearth'])
                if engine != 'postgresql':
                    assert d['allocator_before'] == d['allocator_after'] == 'hearth'
                assert d['limits'] == {'cpu.max': '400000 100000', 'memory.max': '2147483648', 'memory.swap.max': '0', 'pids.max': '1024'}
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



def verify_extra(data):
    read=lambda name:json.loads(data[name])
    build=read('standby-handoff-build-20261030/report.json')
    assert build['passed'] and all(x['exit']==0 for x in build['commands'])
    assert [x['label'] for x in build['commands']]==['allocator-handoff','worker-retirement','release-build']
    assert hashlib.sha256(data['runtime-source/candidate-launch_backend.rs']).hexdigest()==build['source_sha256']
    assert build['binaries']['postgres']['sha256']==BINARIES['rustsql']
    assert b'test result: ok. 2 passed' in data['standby-handoff-build-20261030/allocator-handoff.log']
    assert b'test result: ok. 3 passed' in data['standby-handoff-build-20261030/allocator-handoff.log']
    assert b'test result: ok. 2 passed' in data['standby-handoff-build-20261030/worker-retirement.log']
    checks=read('standby-handoff-checks-20261030.json')
    assert checks['passed'] and all(x['exit']==0 for x in checks['commands'])
    probe=read('standby-handoff-correctness-20261030/report.json')
    assert probe['passed'] and len(probe['cases'])==2
    assert hashlib.sha256(data['check_standby_handoff.py']).hexdigest()==probe['source_sha256']
    for case in probe['cases']:
        engine='rustsql-baseline' if case['label']=='baseline' else 'rustsql'
        assert case['passed'] and case['binary_sha256']==BINARIES[engine]
        assert case['live_environment']==['PGRUST_ALLOCATOR=hearth','PGRUST_GATHER_TRACE=1']
        assert case['shutdown_exit_codes']==[0] and not case['remaining_pids']
        assert not any(case['after_shutdown']['events'][key] for key in ['oom','oom_kill','max'])
        assert case['after_shutdown']['memory_swap_current']==0
        assert len(case['index_builds'])==3
        assert [s['stage'] for s in case['stages']]==['startup_idle','after_setup','setup_idle','after_parallel_builds']
        assert all(sum(v for k,v in s['threads'].items() if k.startswith('pg:standby:'))==16 for s in case['stages'])
        for trial in case['index_builds']:
            assert trial['result']['rows']==500000 and trial['result']['sha256']==probe['oracle']['sha256']
            assert trial['pooled_tasks']==trial['warm_claims']==2
        log=data['server-logs/'+case['label']+'.log']
        assert log.count(b'GTRACE w.pool.task ')>=6 and log.count(b'GTRACE w.retain.claim_warm ')>=6
    protocol=read('standby-handoff-protocol-20261030.json')
    assert protocol['passed']
    assert protocol['engines']['postgresql']['result']==protocol['engines']['rustsql']['result']
    for engine in ['postgresql','rustsql']:
        p=protocol['engines'][engine]
        assert p['binary_sha256']==BINARIES[engine] and p['shutdown_exit_codes']==[0]
        assert len(p['result']['checks'])==8 and p['result']['committed_updates']==192
    decision=read('standby-medium-20261030/decision.json')
    assert decision['selected'] is False and decision['capacity_run'] is False and decision['runtime_source_restored']
    assert decision['preservation_failures']==['range_p99_ms']
    return {'allocator_tests':5,'worker_retirement_tests':2,'parallel_index_builds':6,'protocol_groups_per_engine':8,'selected':False}

if __name__=='__main__':
    root=Path(__file__).resolve().parent;manifest=json.loads((root/'manifest.json').read_text());payload=(root/'evidence.tar.gz').read_bytes()
    assert hashlib.sha256(payload).hexdigest()==manifest['archive_sha256']
    data={}
    with tarfile.open(fileobj=io.BytesIO(payload),mode='r:gz') as archive:
        for member in archive.getmembers():
            p=PurePosixPath(member.name)
            assert member.isfile() and not p.is_absolute() and '..' not in p.parts and member.name not in data
            content=archive.extractfile(member).read();assert hashlib.sha256(content).hexdigest()==manifest['members'][member.name]
            data[member.name]=content
    assert set(data)==set(manifest['members'])
    result={'performance':review(data),'correctness':verify_extra(data)}
    assert result==json.loads((root/'results.json').read_text())
    print(json.dumps({'replay_passed':True,'members':len(data),'selected':result['correctness']['selected']}))

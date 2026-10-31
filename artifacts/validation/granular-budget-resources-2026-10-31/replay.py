"""Recompute resource-gate evidence from archived bytes without extracting or executing them."""
from pathlib import Path, PurePosixPath
import hashlib
import json
import tarfile

from compare_granular_budget_connections import analyze, validate

if not __debug__:
    raise RuntimeError('Assertions must remain enabled')
CANDIDATE = '635ec740b3cd85c4318eadc87843705032494a49d92d1deeb79b09a8c5ab9e57'
BASELINE = '0be60915b40c9f70f39dcb3a0fd1f8918608a20095f767ec081a5b5320caa277'
POSTGRES = '07b16e1aa4f0e2206c37b339b6e4cc813aa8ab9e3c5516334f753b3d90d0c536'
DIGEST = '1617e17e9ee2e6e0a614c994459558f80ed98ada447da0a83dd521398abd057f'
PREFIX = 'granular-budget-limits-20261031'


class Member:
    def __init__(self, files, name):
        self.files, self.name = files, name

    def __truediv__(self, part):
        return Member(self.files, self.name + '/' + str(part))

    def read_bytes(self):
        return self.files[self.name]

    def read_text(self):
        return self.read_bytes().decode()


def review(files):
    read = lambda name: json.loads(files[name])
    design = read('connection-design.json')
    assert design['candidate_sha256'] == CANDIDATE and design['baseline_sha256'] == BASELINE
    assert design['cpu_median_max_increase_percent'] == design['cpu_including_shutdown_median_max_increase_percent'] == 5
    assert design['repeats'] == 5 and design['memory_mib'] == 2048 and design['no_retry_on_failed_trial']
    connections = analyze(Member(files, 'connections'))
    assert connections == read('connection-review.json')
    for metric in ('cpu_us_per_accepted_startup', 'cpu_us_per_accepted_startup_including_shutdown'):
        assert connections[metric]['median_change_pct'] <= 5
    metadata = connections['metadata']
    headroom = []
    for number in range(1, 4):
        pilot = read(f'pilots/granular-budget-2g-pilot-{number}-20261031.json')
        validate(pilot, metadata, 'candidate')
        headroom.append(2048 - pilot['resources_after_shutdown']['memory_peak'] / 2**20)
    summary = read('limits/' + PREFIX + '-summary.json')
    assert summary['complete'] and summary['passed']
    assert summary['design'] == read('limits/' + PREFIX + '-source/design.json')
    assert summary['design'] == {'memory_mib': [2048, 4096, 8192], 'sessions': ['fresh', 'loader'],
        'engines': ['postgresql', 'rustsql'], 'repeats': 1, 'cycles_per_trial': 12, 'expected_trials': 12,
        'performance_qualified': False, 'outer_memory_mib': 12288, 'swap_mib': 0, 'promoted': False}
    expected = [(m, session, engine) for i, m in enumerate((2048, 4096, 8192))
        for session in ('fresh', 'loader')
        for engine in (('postgresql', 'rustsql') if i % 2 == 0 else ('rustsql', 'postgresql'))]
    assert [(t['memory_mib'], t['session'], t['engine']) for t in summary['trials']] == expected
    peaks = []
    for trial in summary['trials']:
        name = trial['name']
        memory, session, engine = trial['memory_mib'], trial['session'], trial['engine']
        assert name == f'{PREFIX}-{memory}-{session}-{engine}'
        assert trial['exit'] == 0 and trial['passed'] and not trial['error']
        command = read('limits/' + name + '-command.json')
        assert command['exit'] == 0 and '--property=MemoryMax=12G' in command['command']
        d = read('limits/' + name + '.json')
        assert d['passed'] and not d['production_qualified'] and not d['performance_qualified']
        assert d['binary_sha256'] == (CANDIDATE if engine == 'rustsql' else POSTGRES)
        assert d['engine'] == engine and d['rows'] == 1_000_000 and d['cycles'] == 12
        assert d['memory_mib'] == memory and d['reuse_loader_session'] == (session == 'loader')
        assert not d['allocator_sql_observer'] and not d.get('monitor_error') and not d.get('error')
        assert d['durability'] == ['on', 'on'] and d['parent_memory_max'] == str(12 * 2**30)
        assert d['limits']['memory.max'] == str(memory * 2**20) and d['limits']['memory.swap.max'] == '0'
        assert d['limits']['cpu.max'] == 'max 100000'
        assert d['cpu_escape_proof'] == {'allowed': [0, 2, 4, 6], 'escape_rejected': True}
        assert d['cpu_binding'] == {'cpuset.cpus.effective': '0,2,4,6', 'cpu.max': 'max 100000'}
        assert d['live_environment'] == (['PGRUST_ALLOCATOR=hearth'] if engine == 'rustsql' else [])
        assert d['warm_reserve_mib'] is None
        assert d['connection_recovery'] == 1 and d['transaction_status'] == 'IDLE'
        assert d['shutdown_exit_codes'] == [0, 0] and not d['remaining_server_pids']
        assert d['restart_count'] == 1_000_000
        assert d['restart_full_sort']['rows'] == 1_000_000 and d['restart_full_sort']['sha256'] == DIGEST
        assert d['restart_topk']['rows'] == 32 and d['restart_topk']['sha256'] == '9327b0f4bed1f4683b132900ab9d864f94062e1189707b91afec8881a80ca572'
        end = d['resources_after_shutdown']
        assert not any(end['events'][key] for key in ('high', 'max', 'oom', 'oom_kill', 'oom_group_kill'))
        assert end['memory_swap_current'] == 0 and 0 < end['memory_peak'] <= memory * 2**20
        for filename, digest in d['source_lf_sha256'].items():
            assert hashlib.sha256(files['limits/' + PREFIX + '-source/' + filename].replace(b'\r\n', b'\n')).hexdigest() == digest
        assert len(d['queries']) == 12
        for index, query in enumerate(d['queries']):
            assert query['index'] == index and query['passed'] and not query['remaining_temp_files']
            assert (query['work_mem'], query['parallel_limit']) == (('64kB', 0) if index % 2 == 0 else ('16MB', 2))
            assert query['result']['rows'] == 1_000_000 and query['result']['sha256'] == DIGEST
            assert query['cpu_delta']['nr_throttled'] == query['cpu_delta']['throttled_usec'] == 0
            if index % 2 == 0:
                assert sum(f['bytes'] for f in query['spill_files']) > 0
        peaks.append({'memory_mib': memory, 'session': session, 'engine': engine, 'peak_mib': end['memory_peak'] / 2**20})
    return {'evidence_valid': True, 'candidate_sha256': CANDIDATE, 'production_qualified': False,
        'connection_trials': 10, 'accepted_startups': 16000, 'overflow_rejections': 20,
        'startup_cpu_change_percent': connections['cpu_us_per_accepted_startup']['median_change_pct'],
        'startup_cpu_including_shutdown_change_percent': connections['cpu_us_per_accepted_startup_including_shutdown']['median_change_pct'],
        'startup_peak_change_percent': connections['peak_cgroup_mib']['median_change_pct'],
        'additional_2g_pilots': 3, 'pilot_accepted_startups': 4800, 'pilot_headroom_mib': headroom,
        'positive_budget_trials': 12, 'successful_sorts': 144, 'full_restart_checks': 12,
        'positive_budget_peaks': peaks}


def tamper_controls(files):
    cases = [('binary', 'connections/candidate-1.json'), ('driver_throttle', 'connections/candidate-1.json'),
             ('restart_digest', 'limits/' + PREFIX + '-2048-fresh-rustsql.json'),
             ('sort_settings', 'limits/' + PREFIX + '-2048-fresh-rustsql.json')]
    rejected = []
    for case, name in cases:
        original = files[name]
        modified = json.loads(original)
        if case == 'binary':
            modified['binary_sha256'] = '0' * 64
        elif case == 'driver_throttle':
            modified['driver_cpu_delta'][0]['nr_throttled'] = 1
        elif case == 'restart_digest':
            modified['restart_full_sort']['sha256'] = '0' * 64
        else:
            modified['queries'][0]['work_mem'] = '16MB'
        files[name] = json.dumps(modified).encode()
        try:
            review(files)
        except (AssertionError, ValueError):
            rejected.append(case)
        else:
            raise AssertionError('forged evidence accepted: ' + case)
        finally:
            files[name] = original
    return rejected


def main():
    root = Path(__file__).resolve().parent
    manifest = json.loads((root / 'manifest.json').read_text())
    archive = root / 'evidence.tar.xz'
    assert hashlib.sha256(archive.read_bytes()).hexdigest() == manifest['archive_sha256']
    files = {}
    with tarfile.open(archive, 'r:xz') as handle:
        for member in handle:
            path = PurePosixPath(member.name)
            assert member.isfile() and not path.is_absolute() and '..' not in path.parts and member.name not in files
            files[member.name] = handle.extractfile(member).read()
    assert {name: hashlib.sha256(content).hexdigest() for name, content in files.items()} == manifest['members']
    result = review(files)
    assert result == json.loads((root / 'results.json').read_text())
    print(json.dumps({'passed': True, 'recorded_evidence_only': True, **result,
                      'tamper_controls_rejected': tamper_controls(files)}))


if __name__ == '__main__':
    main()

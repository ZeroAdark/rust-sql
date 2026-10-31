"""Verify recorded pressure evidence without extracting or executing archive members."""
from pathlib import Path, PurePosixPath
import copy
import hashlib
import importlib.util
import json
import tarfile

if not __debug__:
    raise RuntimeError('Evidence verification requires enabled assertions')

PG = '07b16e1aa4f0e2206c37b339b6e4cc813aa8ab9e3c5516334f753b3d90d0c536'
BASELINE = '0be60915b40c9f70f39dcb3a0fd1f8918608a20095f767ec081a5b5320caa277'
CANDIDATE = '4d0518fe5c82fead8641d71df6b16a7aba27664d13b6b2d125c657fe2b508482'
SORT = '1617e17e9ee2e6e0a614c994459558f80ed98ada447da0a83dd521398abd057f'
TOP = '9327b0f4bed1f4683b132900ab9d864f94062e1189707b91afec8881a80ca572'
CHECKS = {}

def verify(files):
    catalog = json.loads(files['catalog.json'])
    assert len(catalog['trials']) == 33
    counts = {'trials': 0, 'passed_trials': 0, 'successful_sorts': 0, 'canceled_sorts': 0, 'candidate_passes': 0}
    source_versions = set()
    for entry in catalog['trials']:
        name = entry['name']
        trial = json.loads(files[name + '.json'])
        expected_binary = PG if trial['engine'] == 'postgresql' else CANDIDATE if entry['variant'].startswith('candidate') else BASELINE
        assert trial['binary_sha256'] == expected_binary
        assert trial['cycles'] == 12 and trial['rows'] == 1_000_000
        assert trial['limits'] == {'memory.max': '1073741824', 'memory.swap.max': '0', 'cpu.max': 'max 100000', 'pids.max': '1024'}
        assert trial['cpu_binding'] == {'cpuset.cpus.effective': '0,2,4,6', 'cpu.max': 'max 100000'}
        assert trial['durability'] == ['on', 'on']
        assert trial['oracle']['sort'] == [1_000_000, SORT] and trial['oracle']['topk'] == [32, TOP]
        assert trial['shutdown_exit_codes'] == [0, 0] and not trial['remaining_server_pids']
        assert trial['resources_after_shutdown']['memory_swap_current'] == 0
        assert all(trial['resources_after_shutdown']['events'][k] == 0 for k in ('oom', 'oom_kill', 'oom_group_kill'))
        assert not trial.get('error') and not trial.get('monitor_error')
        assert trial['restart_count'] == 1_000_000
        assert (trial['restart_topk']['rows'], trial['restart_topk']['sha256']) == (32, TOP)
        for filename, sha in trial['source_lf_sha256'].items():
            assert hashlib.sha256(files[name + '-source/' + filename].replace(b'\r\n', b'\n')).hexdigest() == sha
        source_versions.add(trial['source_lf_sha256']['sustained_sort_probe.py'])
        for i, query in enumerate(trial['queries']):
            assert query['index'] == i
            assert (query['work_mem'], query['parallel_limit']) == (('64kB', 0) if i % 2 == 0 else ('16MB', 2))
            assert query['cpu_delta']['nr_throttled'] == query['cpu_delta']['throttled_usec'] == 0
            assert query['seconds'] > 0
            if query['passed']:
                assert (query['result']['rows'], query['result']['sha256']) == (1_000_000, SORT)
                assert not query['remaining_temp_files']
                if i % 2 == 0:
                    assert sum(w['bytes'] for w in query['spill_files']) > 0
                counts['successful_sorts'] += 1
            else:
                assert query['sqlstate'] == '53200' and 'memory_cancel_threshold' in query['error']
                assert i == len(trial['queries']) - 1
                counts['canceled_sorts'] += 1
        complete = len(trial['queries']) == 12 and all(q['passed'] for q in trial['queries'])
        assert trial['passed'] == entry['passed'] == complete
        assert entry['exit'] == (0 if complete else 1)
        assert entry['successful_queries'] == sum(q['passed'] for q in trial['queries'])
        if entry['campaign'] == 'warm-limit-pressure-20261031':
            assert (trial['restart_full_sort']['rows'], trial['restart_full_sort']['sha256']) == (1_000_000, SORT)
            assert trial['connection_recovery'] == 1 and trial['transaction_status'] == 'IDLE'
        if expected_binary == CANDIDATE:
            assert trial['passed']
            counts['candidate_passes'] += 1
            if trial['allocator_sql_observer']:
                bounds = [r[4] for s in trial['ownership'] for r in s['rows'] if r[1:3] == ['pool_reserve', 'bound']]
                targets = [r[4] for s in trial['ownership'] for r in s['rows'] if r[1:3] == ['pool_reserve', 'target']]
                assert len(bounds) == len(targets) == 15
                assert set(bounds) == {64 * 2**20} and max(targets) <= 64 * 2**20
        counts['trials'] += 1
        counts['passed_trials'] += complete
    assert counts['candidate_passes'] == 6 and counts['canceled_sorts'] == 3
    bad = catalog['setup_failure']
    trial = json.loads(files[bad + '.json'])
    assert not trial['passed'] and not trial['queries'] and trial['shutdown_exit_codes'] == [1]
    assert b'is too long (maximum 107 bytes)' in files[bad + '-cluster/server.log']
    build = json.loads(files['warm-limit-build-20261031/report.json'])
    assert build['passed'] and build['binaries']['postgres']['sha256'] == CANDIDATE
    assert build['base_commit'] == 'b3d1a667e200da873aa72fda7530cce5c2c9279f'
    for name, sha in build['source_sha256'].items():
        assert hashlib.sha256(files['warm-limit-build-20261031/sources/' + name]).hexdigest() == sha
    assert {c['label']: c['exit'] for c in build['commands']} == {
        'pagepool': 0, 'negative-control': 101, 'restored-limit': 0, 'memruntime': 0, 'release-build': 0}
    assert b'the reserve must fit the published memory limit' in files['warm-limit-build-20261031/negative-control.log']
    for phase, module in CHECKS.items():
        label = 'warm-limit-' + phase + '-20261031'
        reviewed = module.review(files)[label]
        assert reviewed['timed_performance_valid']
        timed = reviewed['timed']
        assert not timed['preservation_failures']
        assert all(timed['candidate_change_percent'][key] > 0 for key in ('charged_peak_mib', 'pss_peak_mib'))
    assessment = json.loads(files['retention-assessment.json'])
    assert assessment['decision'] == 'rejected' and assessment['promoted'] is False
    assert assessment['accepted_binary'] == BASELINE and assessment['rejected_binary'] == CANDIDATE
    return counts | {'harness_setup_failures': 1, 'probe_versions': len(source_versions), 'performance_trials': 25,
                     'timed_sql_statements': 1_761_300, 'candidate_retained': False}

def main():
    root = Path(__file__).resolve().parent
    for phase in ('medium', 'capacity'):
        specification = importlib.util.spec_from_file_location(phase + '_checks', root / (phase + '_checks.py'))
        module = importlib.util.module_from_spec(specification)
        specification.loader.exec_module(module)
        module.BINARIES = {'rustsql-baseline': BASELINE, 'rustsql': CANDIDATE}
        if phase == 'medium':
            module.BINARIES['postgresql'] = PG
        module.CAMPAIGNS = {'warm-limit-' + phase + '-20261031': (None, phase, 30 if phase == 'medium' else 1500,
                                                               9.4 if phase == 'medium' else None)}
        CHECKS[phase] = module
    manifest = json.loads((root / 'manifest.json').read_text())
    archive = root / 'evidence.tar.xz'
    assert hashlib.sha256(archive.read_bytes()).hexdigest() == manifest['archive_sha256']
    files = {}
    with tarfile.open(archive, 'r:xz') as handle:
        for member in handle:
            assert member.isfile() and not PurePosixPath(member.name).is_absolute() and '..' not in PurePosixPath(member.name).parts
            assert member.name not in files
            files[member.name] = handle.extractfile(member).read()
    assert {name: hashlib.sha256(content).hexdigest() for name, content in files.items()} == manifest['members']
    result = verify(files)
    smoke_manifest = json.loads((root / 'standalone-smoke-manifest.json').read_text())
    smoke_archive = root / 'standalone-smoke.tar.xz'
    assert hashlib.sha256(smoke_archive.read_bytes()).hexdigest() == smoke_manifest['archive_sha256']
    smoke_files = {}
    with tarfile.open(smoke_archive, 'r:xz') as handle:
        for member in handle:
            assert member.isfile() and not PurePosixPath(member.name).is_absolute() and '..' not in PurePosixPath(member.name).parts
            assert member.name not in smoke_files
            smoke_files[member.name] = handle.extractfile(member).read()
    assert {name: hashlib.sha256(content).hexdigest() for name, content in smoke_files.items()} == smoke_manifest['members']
    for engine, executable in (('postgresql', PG), ('rustsql', BASELINE)):
        smoke = json.loads(smoke_files['standalone-sort-smoke-20261031-' + engine + '.json'])
        assert smoke['passed'] and smoke['binary_sha256'] == executable and len(smoke['queries']) == 2
        assert smoke['cpu_escape_proof'] == {'allowed': [0, 2, 4, 6], 'escape_rejected': True}
        for query in smoke['queries']:
            assert query['passed'] and query['result']['rows'] == 1_000_000 and query['result']['sha256'] == SORT
        assert smoke['restart_count'] == 1_000_000 and smoke['restart_full_sort']['sha256'] == SORT
        assert smoke['shutdown_exit_codes'] == [0, 0] and not smoke['remaining_server_pids']
        for filename, sha in smoke['source_lf_sha256'].items():
            assert hashlib.sha256(smoke_files['standalone-sort-smoke-20261031-source/' + filename].replace(b'\r\n', b'\n')).hexdigest() == sha
    result['standalone_smoke_trials'] = 2
    sample = 'warm-limit-pressure-20261031-1-candidate-observed.json'
    original = files[sample]
    for mutation in ('row_count', 'binary', 'affinity'):
        trial = json.loads(original)
        if mutation == 'row_count':
            trial['queries'][0]['result']['rows'] -= 1
        elif mutation == 'binary':
            trial['binary_sha256'] = '0' * 64
        else:
            trial['cpu_binding']['cpuset.cpus.effective'] = '0-15'
        files[sample] = json.dumps(trial).encode()
        try:
            verify(files)
        except AssertionError:
            pass
        else:
            raise AssertionError('forged ' + mutation + ' was accepted')
        finally:
            files[sample] = original
    print(json.dumps({'passed': True, 'recorded_evidence_only': True, **result,
                      'negative_controls_rejected': ['row_count', 'binary', 'affinity']}))

if __name__ == '__main__':
    main()

"""Verify frozen granular-budget evidence without rerunning SQL or executing archived code."""
from pathlib import Path, PurePosixPath
import hashlib
import json
import re
import tarfile

if not __debug__:
    raise RuntimeError('Evidence verification requires enabled assertions')

from review_granular_budget_performance import review as review_performance
from review_known_budget_performance import BINARIES as REFERENCES

CANDIDATE = '635ec740b3cd85c4318eadc87843705032494a49d92d1deeb79b09a8c5ab9e57'
BINARIES = REFERENCES | {'rustsql': CANDIDATE}
SORT = '1617e17e9ee2e6e0a614c994459558f80ed98ada447da0a83dd521398abd057f'


def pressure(files):
    summary = json.loads(files['granular-budget-pressure-20261031-summary.json'])
    assert summary['complete'] and len(summary['trials']) == 12
    counts = dict(trials=12, passed_trials=0, successful_sorts=0, watchdog_cancellations=0, oom_trials=0, candidate_passes=0)
    for entry in summary['trials']:
        name = entry['name']
        d = json.loads(files[name + '.json'])
        candidate = entry['variant'].startswith('candidate')
        engine = 'rustsql' if candidate else 'postgresql' if entry['variant'] == 'postgresql' else 'rustsql-baseline'
        assert d['binary_sha256'] == BINARIES[engine]
        assert d['limits'] == {'memory.max': str(2**30), 'memory.swap.max': '0', 'cpu.max': 'max 100000', 'pids.max': '1024'}
        assert d['cpu_binding'] == {'cpuset.cpus.effective': '0,2,4,6', 'cpu.max': 'max 100000'}
        assert d['cpu_escape_proof'] == {'allowed': [0, 2, 4, 6], 'escape_rejected': True}
        assert d['parent_memory_max'] == str(4 * 2**30) and d['memory_mib'] == 1024
        assert d['durability'] == ['on', 'on'] and d['cycles'] == 12 and d['rows'] == 1_000_000
        assert d['reuse_loader_session'] and d['allocator_sql_observer'] == entry['variant'].endswith('-observed')
        assert not d.get('error') and not d.get('monitor_error') and not d['remaining_server_pids']
        assert d['resources_after_shutdown']['memory_swap_current'] == 0
        events = d['resources_after_shutdown']['events']
        oom = bool(events['oom_group_kill'])
        if oom:
            assert entry['variant'] == 'baseline-observed' and events['oom_kill'] == 3 and events['oom_group_kill'] == 1
            assert d['shutdown_exit_codes'] == [-9, 0]
            counts['oom_trials'] += 1
        else:
            assert all(events[event] == 0 for event in ('oom', 'oom_kill', 'oom_group_kill'))
            assert d['shutdown_exit_codes'] == [0, 0]
            assert d['connection_recovery'] == 1 and d['transaction_status'] == 'IDLE'
        assert d['restart_count'] == 1_000_000
        assert (d['restart_full_sort']['rows'], d['restart_full_sort']['sha256']) == (1_000_000, SORT)
        for filename, sha in d['source_lf_sha256'].items():
            assert hashlib.sha256(files[name + '-source/' + filename].replace(b'\r\n', b'\n')).hexdigest() == sha
        for index, q in enumerate(d['queries']):
            assert q['index'] == index
            assert (q['work_mem'], q['parallel_limit']) == (('64kB', 0) if index % 2 == 0 else ('16MB', 2))
            assert q['cpu_delta']['nr_throttled'] == q['cpu_delta']['throttled_usec'] == 0
            if q['passed']:
                assert (q['result']['rows'], q['result']['sha256']) == (1_000_000, SORT)
                assert not q['remaining_temp_files']
                if index % 2 == 0:
                    assert sum(f['bytes'] for f in q['spill_files']) > 0
                counts['successful_sorts'] += 1
            else:
                assert entry['variant'] == 'baseline-observed' and index == len(d['queries']) - 1
                if oom:
                    assert q['sqlstate'] is None and 'server closed the connection unexpectedly' in q['error']
                else:
                    assert q['sqlstate'] == '53200' and 'memory_cancel_threshold' in q['error']
                    counts['watchdog_cancellations'] += 1
        complete = len(d['queries']) == 12 and all(q['passed'] for q in d['queries'])
        assert complete == d['passed'] == entry['passed']
        assert entry['exit'] == (0 if complete else 1)
        counts['passed_trials'] += complete
        counts['candidate_passes'] += candidate and complete
        if candidate or engine == 'postgresql':
            assert complete
        if candidate and d['allocator_sql_observer']:
            guards = [r for r in d['ownership'][-1]['rows'] if r[1:3] == ['pool', 'grow_guard']]
            assert len(guards) == 1 and guards[0][3] > 0 and 0 < guards[0][4] <= guards[0][3] * 16 * 2**20
    assert counts == dict(trials=12, passed_trials=9, successful_sorts=119, watchdog_cancellations=1, oom_trials=2, candidate_passes=6)
    return counts


def verify(files):
    build = json.loads(files['granular-budget-build-20261031/report.json'])
    assert build['passed'] and not build['promoted'] and build['binaries']['postgres']['sha256'] == CANDIDATE
    assert build['base_commit'] == 'b3d1a667e200da873aa72fda7530cce5c2c9279f'
    assert len(build['source_sha256']) == 7
    for name, sha in build['source_sha256'].items():
        assert hashlib.sha256(files['granular-budget-build-20261031/sources/' + name]).hexdigest() == sha
    assert {c['label']: c['exit'] for c in build['commands']} == {
        'focused': 0, 'negative-zero': 101, 'negative-prefault': 101, 'negative-granularity': 101,
        'pagepool': 0, 'memruntime': 0, 'release-build': 0}
    total = 0
    for suite, expected in [('pagepool', 119), ('memruntime', 62)]:
        tests = re.findall(rb'test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored', files[f'granular-budget-build-20261031/{suite}.log'])
        assert sum(int(t[0]) for t in tests) == expected and sum(int(t[1]) for t in tests) == 0
        total += expected
    for label, message in [
        ('negative-zero', b'known or finite budget must release free pages before growth'),
        ('negative-prefault', b'known-zero budget must hold cold prefaults'),
        ('negative-granularity', b'committed=4096 released=2097152')]:
        log = files[f'granular-budget-build-20261031/{label}.log']
        assert b'test result: FAILED.' in log and message in log
    diagnostic = json.loads(files['known-budget-granularity-diagnostic-20261031/report.json'])
    assert diagnostic['test_failed_as_expected'] and not diagnostic['range_latency_causality_proven']
    assert hashlib.sha256(files['known-budget-granularity-diagnostic-20261031/reclaim_granularity.rs']).hexdigest() == diagnostic['test_sha256']
    assert b'committed=4096 released=2097152' in files['known-budget-granularity-diagnostic-20261031/negative.log']
    for name, sha in diagnostic['source_sha256'].items():
        assert hashlib.sha256(files['known-budget-build-20261031/sources/' + name]).hexdigest() == sha
    failed_setup = json.loads(files['granular-budget-build-setup-failure-20261031/setup-failure.json'])
    assert failed_setup['terminal_exit'] == 1 and not failed_setup['tests_or_build_started']
    performance = review_performance(files.__getitem__)
    saved = json.loads(files['granular-budget-performance-review-20261031.json'])
    assert performance == saved and saved['complete']
    return {'unit_tests': total, 'pressure': pressure(files), 'performance': performance}


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
    result = verify(files)
    assert json.loads((root / 'results.json').read_text()) == result['performance']
    rejected = list(result['performance']['tamper_controls_rejected'])
    for case, name in [('zero_reclaim', 'granular-budget-pressure-20261031-1-candidate-observed.json'),
                       ('oom_removal', 'granular-budget-pressure-20261031-1-baseline-observed.json')]:
        original = files[name]
        modified = json.loads(original)
        if case == 'zero_reclaim':
            next(r for r in modified['ownership'][-1]['rows'] if r[1:3] == ['pool', 'grow_guard'])[4] = 0
        else:
            modified['resources_after_shutdown']['events']['oom_group_kill'] = 0
        files[name] = json.dumps(modified).encode()
        try:
            pressure(files)
        except AssertionError:
            rejected.append(case)
        else:
            raise AssertionError('forged evidence accepted: ' + case)
        finally:
            files[name] = original
    assessment = json.loads((root / 'retention-assessment.json').read_text())
    assert not assessment['promoted'] and not assessment['production_qualified']
    passed = result['performance']['preservation_passed']
    assert assessment['performance_preservation_passed'] == passed
    assert assessment['decision'] == ('advance-to-runtime-validation' if passed else 'rejected')
    print(json.dumps({'passed': True, 'recorded_evidence_only': True, 'unit_tests': result['unit_tests'],
                      'pressure': result['pressure'],
                      'performance_trials': sum(p['trial_count'] for p in result['performance']['phases'].values()),
                      'timed_statements': result['performance']['timed_statements'],
                      'performance_preservation_passed': passed, 'tamper_controls_rejected': rejected,
                      'candidate_promoted': False}))


if __name__ == '__main__':
    main()

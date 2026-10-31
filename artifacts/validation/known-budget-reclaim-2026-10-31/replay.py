"""Verify recorded evidence only, without extracting or executing archived code."""
from pathlib import Path, PurePosixPath
import hashlib
import json
import re
import tarfile

if not __debug__:
    raise RuntimeError('Evidence verification requires enabled assertions')

from performance_checks import BINARIES, PHASES, review_phase, tamper_controls

SORT = '1617e17e9ee2e6e0a614c994459558f80ed98ada447da0a83dd521398abd057f'


def pressure(files):
    summary = json.loads(files['known-budget-pressure-20261031-summary.json'])
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
        assert all(d['resources_after_shutdown']['events'][event] == 0 for event in ('oom', 'oom_kill', 'oom_group_kill'))
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
                assert q['sqlstate'] == '53200' and 'memory_cancel_threshold' in q['error']
                counts['watchdog_cancellations'] += 1
        complete = len(d['queries']) == 12 and all(q['passed'] for q in d['queries'])
        assert complete == d['passed'] == entry['passed']
        assert entry['exit'] == (0 if complete else 1)
        counts['passed_trials'] += complete
        counts['candidate_passes'] += candidate and complete
        if candidate:
            assert complete
            if d['allocator_sql_observer']:
                guards = [r for r in d['ownership'][-1]['rows'] if r[1:3] == ['pool', 'grow_guard']]
                assert len(guards) == 1 and guards[0][3] > 0 and 0 < guards[0][4] <= guards[0][3] * 16 * 2**20
    assert counts == dict(trials=12, passed_trials=9, successful_sorts=119, watchdog_cancellations=3, oom_trials=0, candidate_passes=6)
    return counts


def verify(files):
    build = json.loads(files['known-budget-build-20261031/report.json'])
    assert build['passed'] and not build['promoted'] and build['binaries']['postgres']['sha256'] == BINARIES['rustsql']
    assert build['base_commit'] == 'b3d1a667e200da873aa72fda7530cce5c2c9279f'
    assert len(build['source_sha256']) == 6
    for name, sha in build['source_sha256'].items():
        assert hashlib.sha256(files['known-budget-build-20261031/sources/' + name]).hexdigest() == sha
    assert {c['label']: c['exit'] for c in build['commands']} == {
        'focused': 0, 'negative-zero': 101, 'negative-prefault': 101, 'pagepool': 0, 'memruntime': 0, 'release-build': 0}
    total = 0
    for suite, expected in [('pagepool', 118), ('memruntime', 62)]:
        tests = re.findall(rb'test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored', files[f'known-budget-build-20261031/{suite}.log'])
        assert sum(int(t[0]) for t in tests) == expected and sum(int(t[1]) for t in tests) == 0
        total += expected
    for control in ('negative-zero', 'negative-prefault'):
        log = files[f'known-budget-build-20261031/{control}.log']
        assert b'test result: FAILED.' in log and b'reclaim_budget' in log
    assert b'known or finite budget must release free pages before growth' in files['known-budget-build-20261031/negative-zero.log']
    assert b'known-zero budget must hold cold prefaults' in files['known-budget-build-20261031/negative-prefault.log']
    phases = {key: review_phase(files.__getitem__, key) for key in PHASES}
    saved = json.loads(files['known-budget-performance-review-20261031.json'])
    assert phases == saved['phases'] and saved['evidence_valid']
    assert saved['timed_statements'] == sum(p['timed_statements'] for p in phases.values()) == 1_812_600
    assert saved['warmup_statements'] == sum(p['warmup_statements'] for p in phases.values()) == 45_600
    passed = all(p['passed'] for p in phases.values())
    assert saved['all_requested_preservation_guards_pass'] == passed
    screen = json.loads(files['known-budget-screen-20261031.json'])
    assert screen['passed'] == passed and not screen['promoted'] and screen['pressure_passed']
    assert screen['stages'] == [
        {'script': 'run_known_budget_pressure.py', 'arguments': [], 'exit': 0},
        {'script': 'run_known_budget_performance.py', 'arguments': ['medium'], 'exit': 0},
        {'script': 'run_known_budget_performance.py', 'arguments': ['capacity'], 'exit': 0},
        {'script': 'run_known_budget_performance.py', 'arguments': ['medium', '1024'], 'exit': 0}]
    return {'unit_tests': total, 'pressure': pressure(files), 'phases': phases, 'performance_preservation_passed': passed}


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
    assert json.loads((root / 'results.json').read_text()) == json.loads(files['known-budget-performance-review-20261031.json'])
    rejected = tamper_controls(files.__getitem__)
    name = 'known-budget-pressure-20261031-1-candidate-observed.json'
    original = files[name]
    modified = json.loads(original)
    guard = next(r for r in modified['ownership'][-1]['rows'] if r[1:3] == ['pool', 'grow_guard'])
    guard[4] = 0
    files[name] = json.dumps(modified).encode()
    try:
        pressure(files)
    except AssertionError:
        rejected.append('zero_reclaim')
    else:
        raise AssertionError('guard inactivity must invalidate the pressure evidence')
    files[name] = original
    assessment = json.loads((root / 'retention-assessment.json').read_text())
    assert not assessment['promoted'] and not assessment['production_qualified']
    assert assessment['performance_preservation_passed'] == result['performance_preservation_passed']
    assert assessment['decision'] == ('advance-to-runtime-validation' if result['performance_preservation_passed'] else 'rejected')
    print(json.dumps({'passed': True, 'recorded_evidence_only': True, 'unit_tests': result['unit_tests'],
                      'pressure': result['pressure'], 'performance_trials': 40, 'timed_statements': 1_812_600,
                      'performance_preservation_passed': result['performance_preservation_passed'],
                      'tamper_controls_rejected': rejected, 'candidate_promoted': False}))


if __name__ == '__main__':
    main()

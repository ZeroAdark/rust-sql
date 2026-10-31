"""Check the recorded failed experiment, preserving cancellations and OOM evidence."""
from pathlib import Path, PurePosixPath
import hashlib
import json
import re
import tarfile

if not __debug__:
    raise RuntimeError('Evidence verification requires enabled assertions')

PG = '07b16e1aa4f0e2206c37b339b6e4cc813aa8ab9e3c5516334f753b3d90d0c536'
BASE = '0be60915b40c9f70f39dcb3a0fd1f8918608a20095f767ec081a5b5320caa277'
CANDIDATE = '73addf2f8955a5db2b2116c26041cdd7e5e6778eabf3589355d20e5ea9776d7b'
SORT = '1617e17e9ee2e6e0a614c994459558f80ed98ada447da0a83dd521398abd057f'

def verify(files):
    summary = json.loads(files['budget-guard-pressure-20261031-summary.json'])
    assert summary['complete'] and len(summary['trials']) == 12
    counts = dict(trials=12, passed_trials=0, successful_sorts=0, watchdog_cancellations=0, oom_trials=0, candidate_passes=0)
    for entry in summary['trials']:
        name = entry['name']
        d = json.loads(files[name + '.json'])
        candidate = entry['variant'].startswith('candidate')
        assert d['binary_sha256'] == (CANDIDATE if candidate else PG if d['engine'] == 'postgresql' else BASE)
        assert d['limits'] == {'memory.max': '1073741824', 'memory.swap.max': '0', 'cpu.max': 'max 100000', 'pids.max': '1024'}
        assert d['cpu_binding'] == {'cpuset.cpus.effective': '0,2,4,6', 'cpu.max': 'max 100000'}
        assert d['cpu_escape_proof'] == {'allowed': [0, 2, 4, 6], 'escape_rejected': True}
        assert d['durability'] == ['on', 'on'] and d['cycles'] == 12 and d['rows'] == 1_000_000
        assert not d.get('error') and not d.get('monitor_error') and not d['remaining_server_pids']
        assert d['resources_after_shutdown']['memory_swap_current'] == 0
        assert d['restart_count'] == 1_000_000
        assert (d['restart_full_sort']['rows'], d['restart_full_sort']['sha256']) == (1_000_000, SORT)
        for filename, sha in d['source_lf_sha256'].items():
            assert hashlib.sha256(files[name + '-source/' + filename].replace(b'\r\n', b'\n')).hexdigest() == sha
        oom = d['resources_after_shutdown']['events']['oom_group_kill']
        assert oom in (0, 1)
        assert d['shutdown_exit_codes'] == ([-9, 0] if oom else [0, 0])
        if oom:
            assert candidate and d['resources_after_shutdown']['events']['oom_kill'] > 0
            counts['oom_trials'] += 1
        else:
            assert not d['resources_after_shutdown']['events']['oom_kill']
            assert d['connection_recovery'] == 1 and d['transaction_status'] == 'IDLE'
        for index, q in enumerate(d['queries']):
            assert q['index'] == index
            assert (q['work_mem'], q['parallel_limit']) == (('64kB', 0) if index % 2 == 0 else ('16MB', 2))
            if q['passed']:
                assert (q['result']['rows'], q['result']['sha256']) == (1_000_000, SORT)
                assert not q['remaining_temp_files']
                if index % 2 == 0:
                    assert sum(w['bytes'] for w in q['spill_files']) > 0
                counts['successful_sorts'] += 1
            else:
                assert index == len(d['queries']) - 1
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
        if candidate and d['allocator_sql_observer']:
            for sample in d['ownership']:
                guards = [r for r in sample['rows'] if r[1:3] == ['pool', 'grow_guard']]
                assert len(guards) == 1 and guards[0][3:7] == [0, 0, 0, 0]
    assert counts == dict(trials=12, passed_trials=4, successful_sorts=93, watchdog_cancellations=6, oom_trials=2, candidate_passes=1)
    build = json.loads(files['budget-guard-build-20261031/report.json'])
    assert build['passed'] and not build['promoted'] and build['binaries']['postgres']['sha256'] == CANDIDATE
    for name, sha in build['source_sha256'].items():
        assert hashlib.sha256(files['budget-guard-build-20261031/sources/' + name]).hexdigest() == sha
    assert {c['label']: c['exit'] for c in build['commands']} == {'focused': 0, 'negative-control': 101, 'pagepool': 0, 'release-build': 0}
    tests = re.findall(rb'test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored', files['budget-guard-build-20261031/pagepool.log'])
    assert sum(int(t[0]) for t in tests) == 118 and sum(int(t[1]) for t in tests) == 0
    assert b'budget must release free pages before growth' in files['budget-guard-build-20261031/negative-control.log']
    diagnostic = json.loads(files['small-budget-semantics-20261031/report.json'])
    assert diagnostic['exit'] == 0 and diagnostic['existing_behavior_only']
    assert hashlib.sha256(files['small-budget-semantics-20261031/small_limit_budget_diagnostic.rs']).hexdigest() == diagnostic['source_sha256']
    assert b'limit_mib=1024 os_reserve_mib=1024 page_cache_floor_mib=64 budget_mib=0' in files['small-budget-semantics-20261031/test.log']
    screen = json.loads(files['budget-guard-screen-20261031.json'])
    assert not screen['passed'] and not screen['promoted'] and screen['rejected_at'] == 'pressure'
    assert len(screen['stages']) == 1
    return counts

def main():
    root = Path(__file__).resolve().parent
    manifest = json.loads((root / 'manifest.json').read_text())
    archive = root / 'evidence.tar.xz'
    assert hashlib.sha256(archive.read_bytes()).hexdigest() == manifest['archive_sha256']
    files = {}
    with tarfile.open(archive, 'r:xz') as handle:
        for member in handle:
            assert member.isfile() and not PurePosixPath(member.name).is_absolute() and '..' not in PurePosixPath(member.name).parts
            assert member.name not in files
            files[member.name] = handle.extractfile(member).read()
    assert {n: hashlib.sha256(b).hexdigest() for n, b in files.items()} == manifest['members']
    counts = verify(files)
    sample = 'budget-guard-pressure-20261031-1-candidate-unobserved.json'
    original = files[sample]
    forged = json.loads(original)
    forged['resources_after_shutdown']['events']['oom_group_kill'] = 0
    files[sample] = json.dumps(forged).encode()
    try:
        verify(files)
    except AssertionError:
        pass
    else:
        raise AssertionError('removing the OOM event must invalidate this evidence')
    decision = json.loads((root / 'retention-assessment.json').read_text())
    assert decision['decision'] == 'rejected' and not decision['promoted'] and not decision['performance_comparison_run']
    print(json.dumps({'passed': True, 'recorded_evidence_only': True, **counts, 'oom_removal_rejected': True}))

if __name__ == '__main__':
    main()

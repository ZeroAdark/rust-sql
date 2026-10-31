"""Verify saved diagnostic evidence without extracting or executing archived files."""
from pathlib import Path, PurePosixPath
import hashlib
import json
import tarfile

BASELINE = '8e5bfc2b093055a5d2257dc817037a2b2f2906faf7c1b83e431f472a2f0513f0'
CANDIDATE = '02cfbca6869bb352c8081b1fb275cdd4317b33a72b8feb05c563408e4c48d454'
PG = '07b16e1aa4f0e2206c37b339b6e4cc813aa8ab9e3c5516334f753b3d90d0c536'


def review(data):
    read = lambda name: json.loads(data[name])
    build = read('build/report.json')
    assert build['passed'] and build['source_restored_after_control']
    assert [(c['label'], c['exit']) for c in build['commands']] == [('negative-control', 101), ('restored-focused', 0), ('release-build', 0)]
    assert build['binaries']['postgres']['sha256'] == CANDIDATE
    assert hashlib.sha256(data['source/runtime.rs']).hexdigest() == build['source_sha256']
    assert b'61 passed; 0 failed' in data['build/unit.log']
    assert b'unknown must not be reported as zero' in data['build/negative-control.log']
    assert b'0 passed; 1 failed' in data['build/negative-control.log']
    assert b'1 passed; 0 failed' in data['build/restored-focused.log']
    equivalent = read('build/reclaim-path-equivalence.json')
    assert equivalent['old_residency_body_equals_new_lower_bound_body']
    assert equivalent['retained_now_only_changes_callee_name']
    assert read('build/share-repair.json')['same_as_retained_runtime']

    failed = read('runs/initial-live.json')
    assert not failed['passed'] and len(failed['cases']) == 3
    assert all(c['passed'] for c in failed['cases'][:2])
    assert not failed['cases'][2]['passed']
    assert 'postgres.bki' in failed['cases'][2]['error']
    assert all(not c['remaining_pids'] for c in failed['cases'])

    live = read('runs/live.json')
    assert live['passed'] and len(live['cases']) == 4
    assert live['kernel_probe'] == failed['kernel_probe']
    assert live['kernel_probe']['supported'] is False
    assert live['kernel_probe']['prctl_return'] == -1 and live['kernel_probe']['errno'] == 22
    assert live['kernel_probe']['visible_in_maps'] is False
    assert {(c['label'], c['allocator']) for c in live['cases']} == {
        ('baseline', 'hearth'), ('baseline', 'mimalloc'), ('candidate', 'hearth'), ('candidate', 'mimalloc')}
    for name, digest in live['source_sha256'].items():
        assert hashlib.sha256(data['source/'+name]).hexdigest() == digest
    for case in live['cases']:
        assert case['passed'] and case['shutdown_exit_codes'] == [0, 0] and not case['remaining_pids']
        assert case['binary_sha256'] == (BASELINE if case['label'] == 'baseline' else CANDIDATE)
        assert case['environment'] == ['PGRUST_ALLOCATOR='+case['allocator']]
        assert case['before_restart'] == case['after_restart']
        assert case['before_restart']['committed_updates'] == 960
        work = case['workload']
        assert work['passed'] and work['clients'] == 6 and work['fact_rows'] == 1000000
        assert work['bundles_per_client'] == 30 and work['warmup_per_client'] == 10
        assert case['after_shutdown']['memory_swap_current'] == 0
        assert all(case['after_shutdown']['events'][k] == 0 for k in ['oom', 'oom_kill', 'max'])
        assert [s['stage'] for s in case['stages']] == ['startup', 'setup', 'after_workload', 'after_restart']
        for stage in case['stages']:
            values = {name: value for name, value, _ in stage['accounts']}
            assert values['resident'] > 0 and values['resident_cgroup'] > 0
            unavailable = {'allocator_resident', 'allocator_retained', 'allocator_live'}
            if case['label'] == 'candidate':
                assert unavailable.isdisjoint(values) and not stage['mimalloc_owner_rows']
            else:
                assert unavailable <= values.keys()
                if case['allocator'] == 'mimalloc':
                    assert values['allocator_resident'] == 0

    protocol = read('runs/protocol.json')
    assert protocol['passed']
    p, c = protocol['engines']['postgresql'], protocol['engines']['rustsql']
    assert p['binary_sha256'] == PG and c['binary_sha256'] == CANDIDATE
    assert p['result'] == c['result'] and len(c['result']['checks']) == 8
    assert p['shutdown_exit_codes'] == c['shutdown_exit_codes'] == [0]
    spill = read('runs/spill.json')
    assert spill['passed'] and spill['rows'] == 500000 and spill['memory_mib'] == 1024
    assert spill['servers']['reference']['binary_sha256'] == PG
    assert spill['servers']['candidate']['binary_sha256'] == CANDIDATE
    for server in spill['servers'].values():
        assert server['passed'] and server['shutdown_exit_codes'] == [0]
        assert server['durability'] == ['on', 'on'] and len(server['cases']) == 6
        for case in server['cases']:
            assert case['passed'] and not case['remaining_temp_files']
            assert [case['result']['rows'], case['result']['sha256']] == spill['oracle'][case['kind']]
            if case['required_spill']:
                assert sum(f['bytes'] for f in case['temp_files']) > 0
        assert [e['sqlstate'] for e in server['temp_limit_errors']] == ['53400', '53400']
        assert [e['sqlstate'] for e in server['cancellations']] == ['57014', '57014']
        assert all(e['live_spill_files'] for e in server['cancellations'])
        assert not server['remaining_server_pids']
        assert server['resources_after_shutdown']['memory_swap_current'] == 0
        assert not any(server['resources_after_shutdown']['events'][k] for k in ['oom', 'oom_kill', 'oom_group_kill'])
    checks = read('runs/checks.json')
    assert checks['passed'] and [(c['label'], c['exit']) for c in checks['commands']] == [('accounting', 0), ('protocol', 0), ('spill', 0)]
    return {'unit_tests': 61, 'negative_control_failed_as_expected': True,
            'complete_live_cases': 4, 'checked_statements_including_warmup': 4*6*40*19,
            'committed_updates_verified_after_restart': 4*960, 'protocol_groups_per_engine': 8,
            'spill_rows': 500000, 'spill_memory_mib': 1024,
            'naming_supported_host_exercised': False, 'new_performance_qualification': False,
            'production_qualified': False}


def main():
    if not __debug__:
        raise RuntimeError('assertions are required')
    root = Path(__file__).resolve().parent
    manifest = json.loads((root/'manifest.json').read_text())
    archive = root/'evidence.tar.gz'
    assert hashlib.sha256(archive.read_bytes()).hexdigest() == manifest['archive_sha256']
    entries = manifest['files']
    assert len({x['path'] for x in entries}) == len(entries)
    data = {}
    with tarfile.open(archive, 'r:gz') as tar:
        members = tar.getmembers()
        assert len(members) == len(entries) < 500 and sum(m.size for m in members) < 50000000
        expected = {x['path']: x for x in entries}
        for member in members:
            name = member.name
            path = PurePosixPath(name)
            assert member.isfile() and not path.is_absolute() and '..' not in path.parts
            assert name not in data and name in expected
            content = tar.extractfile(member).read()
            assert len(content) == expected[name]['size']
            assert hashlib.sha256(content).hexdigest() == expected[name]['sha256']
            data[name] = content
    result = review(data)
    assert result == json.loads((root/'results.json').read_text())
    print(json.dumps({'verified_members': len(data), **result}, indent=2))


if __name__ == '__main__':
    main()

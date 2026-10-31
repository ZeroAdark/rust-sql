"""Check the rejected experiment's archive without extracting or executing its contents."""
from pathlib import Path, PurePosixPath
import hashlib
import io
import json
import re
import tarfile
from connection_checks import analyze

if not __debug__:
    raise SystemExit('assertions required')

root = Path(__file__).resolve().parent
manifest = json.loads((root / 'manifest.json').read_text())
blob = (root / 'evidence.tar.gz').read_bytes()
assert hashlib.sha256(blob).hexdigest() == manifest['archive_sha256']
members = {}
with tarfile.open(fileobj=io.BytesIO(blob), mode='r:gz') as archive:
    for entry in archive:
        path = PurePosixPath(entry.name)
        assert entry.isfile() and not path.is_absolute() and '..' not in path.parts
        assert entry.name not in members
        members[entry.name] = archive.extractfile(entry).read()
assert set(members) == set(manifest['members'])
for name, data in members.items():
    assert hashlib.sha256(data).hexdigest() == manifest['members'][name], name


class Archived:
    def __init__(self, name):
        self.name = name

    def __truediv__(self, name):
        return Archived(self.name + '/' + str(name))

    def read_bytes(self):
        return members[self.name]

    def read_text(self):
        return self.read_bytes().decode()


def read(name):
    return json.loads(members[name])


for external, archived in {
    'candidate-manifest.json': 'candidate/manifest.json',
    'decision.json': 'candidate/decision.json',
    'restoration.json': 'candidate/restoration.json',
    'first-campaign-validation.json': 'campaign1/performance-validation.json',
}.items():
    assert json.loads((root / external).read_text()) == read(archived)


# Preserve the invalid campaign and prove the normal validator refuses it.
try:
    analyze(Archived('campaign1'))
except ValueError as error:
    assert str(error) == 'CPU throttled'
else:
    raise AssertionError('throttled first campaign was accepted')
# This checks every other invariant without rewriting any recorded counter.
analyze(Archived('campaign1'), allow_throttle=True)
invalid = read('campaign1/performance-validation.json')
assert invalid['evidence_valid'] is False
assert {x['report']: x['cpu_delta']['throttled_usec'] for x in invalid['invalid_trials']} == {
    'baseline-4.json': 613, 'candidate-5.json': 38685,
}
second = analyze(Archived('campaign2'))
assert second == json.loads((root / 'connection-results.json').read_text())
assert second['all_pairs_more_cpu']
assert 24.9 < second['cpu_us_per_accepted_startup']['median_change_pct'] < 25
assert -13.4 < second['peak_cgroup_mib']['median_change_pct'] < -13.3

candidate = read('candidate/manifest.json')
assert len(candidate['runtime_source_sha256']) == 8
for name, sha in candidate['runtime_source_sha256'].items():
    assert hashlib.sha256(members['candidate/source/' + name]).hexdigest() == sha
assert hashlib.sha256(members['candidate/candidate.patch']).hexdigest() == candidate['runtime_patch_sha256']
log = members['candidate/small-superpage-unlocked-tests.log'].decode()
counts = re.findall(r'test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;', log)
assert len(counts) == 85 and sum(int(x[0]) for x in counts) == 117
assert sum(int(x[1]) for x in counts) == 0 and sum(int(x[2]) for x in counts) == 1
negative = read('candidate/small-superpage-unlocked-negative-control.json')
assert negative['passed'] and negative['positive_restored'] and negative['exit'] == 101
assert negative['positive_source_sha256'] == candidate['runtime_source_sha256']['crates/_support/pagepool/src/state.rs']
assert negative['expected_failure'] in members['candidate/small-superpage-unlocked-negative-control.log'].decode()
assert b'Finished `release` profile' in members['candidate/small-superpage-unlocked-build.log']
protocol = read('support/small-superpage-unlocked-candidate-correctness.json')
assert protocol['passed']
assert protocol['engines']['rustsql']['binary_sha256'] == candidate['binaries']['postgres']
assert protocol['engines']['postgresql']['result'] == protocol['engines']['rustsql']['result']
assert len(protocol['engines']['rustsql']['result']['checks']) == 8
assert protocol['engines']['rustsql']['result']['committed_updates'] == 192
assert all(x['shutdown_exit_codes'] == [0] for x in protocol['engines'].values())
assert read('candidate/decision.json')['accepted'] is False
restoration = read('candidate/restoration.json')
assert set(restoration['source_files_restored']) == set(candidate['runtime_source_sha256'])
assert restoration['source_hashes_verified_before_restore'] and restoration['target_binary_copied_from_retained_not_rebuilt']
assert restoration['retained_binary_sha256'] != candidate['binaries']['postgres']
print(json.dumps({'passed': True, 'hashed_members': len(members), 'accepted_startups': 32000,
                  'valid_performance_pairs': 5, 'rejected_campaigns': 1, 'production_qualified': False}))

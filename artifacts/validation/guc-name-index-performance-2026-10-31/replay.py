"""Verify recorded comparisons; never execute code from the evidence archive."""
from pathlib import Path, PurePosixPath
import copy, hashlib, io, json, tarfile
import quota_checks, cpuset_checks, connection_checks

if not __debug__:raise RuntimeError('Verification requires enabled assertions')

MEDIUM='guc-name-index-medium-20261031'
REJECTED='guc-name-index-capacity-20261031'
CAPACITY='guc-name-index-cpuset-capacity-20261031'
CONNECTIONS='guc-name-index-connections-20261031'

class RecordedPath:
    def __init__(self, data, name):self.data,self.name=data,name
    def __truediv__(self, other):return RecordedPath(self.data,self.name+'/'+str(other))
    def read_bytes(self):return self.data[self.name]
    def read_text(self):return self.read_bytes().decode()

def review(data):
    quota_checks.CAMPAIGNS={MEDIUM:(None,'medium',30,9.4),REJECTED:(None,'capacity',50,None)}
    quota_checks.REJECTED={REJECTED:{'capacity-postgresql-1.json':(33,113561),'capacity-postgresql-2.json':(54,16386),'capacity-postgresql-3.json':(18,21200)}}
    old=quota_checks.review(data,allow_recorded_throttle=True)
    assert old[MEDIUM]['timed_performance_valid'] and not old[MEDIUM]['timed']['preservation_failures']
    assert not old[REJECTED]['timed_performance_valid']
    cpuset_checks.CAMPAIGNS={CAPACITY:(None,'capacity',50,None)}
    new=cpuset_checks.review(data)
    assert new[CAPACITY]['timed_performance_valid'] and not new[CAPACITY]['timed']['preservation_failures']
    for campaign in (old[MEDIUM],new[CAPACITY]):
        for window in ('warmup_and_measurement_cpu','setup_warmup_and_measurement_cpu'):
            assert campaign[window]['rust_comparison_valid'] and campaign[window]['median_change_pct']<=5
    connections=connection_checks.analyze(RecordedPath(data,CONNECTIONS))
    assert connections['evidence_valid']
    for metric in ('cpu_us_per_accepted_startup','cpu_us_per_accepted_startup_including_shutdown'):
        assert connections[metric]['median_change_pct']<=5
    assert connections['peak_cgroup_mib']['median_change_pct']<0
    metadata=copy.deepcopy(connections['metadata']);metadata['memory_mib']=2048
    pilots=[]
    for i in range(1,4):
        report=json.loads(data[f'pilots/guc-name-index-2g-pilot-{i}-20261031.json'])
        connection_checks.validate(report,metadata,'candidate')
        assert report['limits']['memory.max']=='2147483648'
        pilots.append(report['resources_after_shutdown']['memory_peak']/2**20)
    return {'medium':old[MEDIUM], 'capacity':new[CAPACITY],
            'rejected_quota_capacity':old[REJECTED], 'connections':connections,
            'two_gib_pilot_peak_mib':pilots, 'production_qualified':False}

def load(root):
    manifest=json.loads((root/'manifest.json').read_text())
    payload=(root/'evidence.tar.gz').read_bytes()
    assert hashlib.sha256(payload).hexdigest()==manifest['archive_sha256']
    data={}
    with tarfile.open(fileobj=io.BytesIO(payload),mode='r:gz') as archive:
        for member in archive.getmembers():
            path=PurePosixPath(member.name)
            assert member.isfile() and not path.is_absolute() and '..' not in path.parts and member.name not in data
            value=archive.extractfile(member).read()
            assert hashlib.sha256(value).hexdigest()==manifest['members'][member.name]
            data[member.name]=value
    assert set(data)==set(manifest['members'])
    return data

if __name__=='__main__':
    root=Path(__file__).resolve().parent
    result=review(load(root))
    assert result==json.loads((root/'results.json').read_text())
    print(json.dumps({'replay_passed':True,'valid_mixed_trials':30,'excluded_mixed_trials':15,'accepted_comparison_startups':16000,'two_gib_pilot_startups':4800,'production_qualified':False}))

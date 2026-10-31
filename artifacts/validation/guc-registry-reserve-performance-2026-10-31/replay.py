"""Verify recorded comparisons; never execute code from the evidence archive."""
from pathlib import Path, PurePosixPath
import copy, hashlib, io, json, tarfile
import cpuset_checks, long_checks, connection_checks

if not __debug__:raise RuntimeError('Verification requires enabled assertions')

MEDIUM='guc-registry-reserve-medium-20261031'
CAPACITY='guc-registry-reserve-capacity-20261031'
LONG='guc-registry-reserve-long-capacity-20261031'
CONNECTIONS='guc-registry-reserve-connections-20261031'

class RecordedPath:
    def __init__(self, data, name):self.data,self.name=data,name
    def __truediv__(self, other):return RecordedPath(self.data,self.name+'/'+str(other))
    def read_bytes(self):return self.data[self.name]
    def read_text(self):return self.read_bytes().decode()

def review(data):
    cpuset_checks.CAMPAIGNS={MEDIUM:(None,'medium',30,9.4),CAPACITY:(None,'capacity',50,None)}
    short=cpuset_checks.review(data)
    assert short[MEDIUM]['timed_performance_valid'] and not short[MEDIUM]['timed']['preservation_failures']
    assert short[CAPACITY]['timed_performance_valid']
    assert short[CAPACITY]['timed']['preservation_failures']==['range_p99_ms']
    long_checks.CAMPAIGNS={LONG:(None,'capacity',1500,None)}
    long=long_checks.review(data)[LONG]
    assert long['timed_performance_valid'] and not long['timed']['preservation_failures']
    for campaign in (*short.values(),long):
        for window in ('warmup_and_measurement_cpu','setup_warmup_and_measurement_cpu'):
            assert campaign[window]['rust_comparison_valid'] and campaign[window]['median_change_pct']<=5
        assert all(campaign['timed']['candidate_change_percent'][key]<0 for key in ('charged_peak_mib','pss_peak_mib'))
    connections=connection_checks.analyze(RecordedPath(data,CONNECTIONS))
    assert connections['evidence_valid']
    for metric in ('cpu_us_per_accepted_startup','cpu_us_per_accepted_startup_including_shutdown','peak_cgroup_mib'):
        assert connections[metric]['median_change_pct']<0
        assert connections[metric]['paired_change_pct']['max']<0
    metadata=copy.deepcopy(connections['metadata']);metadata['memory_mib']=2048
    pilots=[]
    for i in range(1,4):
        report=json.loads(data[f'pilots/guc-registry-reserve-2g-pilot-{i}-20261031.json'])
        connection_checks.validate(report,metadata,'candidate')
        assert report['limits']['memory.max']=='2147483648'
        pilots.append(report['resources_after_shutdown']['memory_peak']/2**20)
    followup=json.loads(data['drivers/guc-registry-reserve-followup-20261031.json'])
    assert followup['passed'] and followup['short_capacity_guard_passed'] is False and followup['long_capacity_guard_passed'] is True
    return {'medium':short[MEDIUM], 'short_capacity':short[CAPACITY], 'long_capacity_rust_only':long,
            'short_capacity_guard_passed':False,'long_capacity_guard_passed':True,
            'connections':connections,'two_gib_pilot_peak_mib':pilots,'production_qualified':False}

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
    print(json.dumps({'replay_passed':True,'valid_mixed_trials':40,'short_capacity_guard_passed':False,'long_capacity_guard_passed':True,'accepted_comparison_startups':16000,'two_gib_pilot_startups':4800,'production_qualified':False}))

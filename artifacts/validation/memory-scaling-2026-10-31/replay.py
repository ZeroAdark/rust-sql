"""Verify the saved experiment; never extract/execute archived source or rerun SQL."""
from collections import Counter
import hashlib,io,json
from pathlib import Path,PurePosixPath
import tarfile

if not __debug__:
    raise RuntimeError('Run without -O/-OO or PYTHONOPTIMIZE; verification requires assertions.')

root=Path(__file__).resolve().parent
manifest=json.loads((root/'manifest.json').read_text())
archive=(root/'evidence.tar.gz').read_bytes()
assert hashlib.sha256(archive).hexdigest()==manifest['archive_sha256']
assert len(archive)==manifest['archive_bytes']
contents={}
with tarfile.open(fileobj=io.BytesIO(archive),mode='r:gz') as tar:
    for member in tar:
        name=PurePosixPath(member.name)
        assert member.isfile() and not name.is_absolute() and '..' not in name.parts and member.name not in contents
        expected=manifest['members'][member.name]
        assert member.size==expected['bytes']
        data=tar.extractfile(member).read()
        assert hashlib.sha256(data).hexdigest()==expected['sha256']
        contents[member.name]=data
assert set(contents)==set(manifest['members'])
for name,digest in manifest['outer_sha256'].items():
    assert hashlib.sha256((root/name).read_bytes()).hexdigest()==digest,name

identities={'postgresql':'07b16e1aa4f0e2206c37b339b6e4cc813aa8ab9e3c5516334f753b3d90d0c536',
            'rustsql':'02cfbca6869bb352c8081b1fb275cdd4317b33a72b8feb05c563408e4c48d454'}
expected_types={'read':[23,20],'update':[23,20],'aggregate':[23,20,20],'range':[20,20],'join':[23,20,20]}
statements={'read':12,'update':4,'aggregate':1,'range':1,'join':1,'bundle':1}
def read(name):return json.loads(contents[name])
def updates(clients,warmup,bundles):
    values=Counter();shard=120000//clients
    for client in range(clients):
        for number in range(warmup+bundles):
            for phase in range(4):values[client*shard+(number*67+phase*19)%shard+1]+=1
    vector=sorted(values.items())
    return {'changed_accounts':len(vector),'committed_updates':clients*(warmup+bundles)*4,
            'values_sha256':hashlib.sha256(json.dumps(vector).encode()).hexdigest()}
count=passed=failed=0
for campaign in manifest['campaigns']:
    prefix=campaign['prefix'];meta=read(prefix+'/metadata.json');summary=read(prefix+'/summary.json')
    assert summary['complete'] and not summary['production_qualified']
    assert meta['binary_sha256']==identities
    if campaign['kind'] in ('connections','restart'):assert meta['assertions_enabled']
    if campaign['kind']=='data':
        observation=read(prefix+'/assertions-runtime-observation.json')
        assert not observation['optimization_environment'] and '-O' not in observation['argv'] and '-OO' not in observation['argv']
    for name,digest in meta['source_sha256'].items():
        assert hashlib.sha256(contents[campaign['source_prefix']+'/'+name]).hexdigest()==digest
    assert read(campaign['command'])['exit']==0
    pairs=set()
    for item in summary['trials']:
        key=(item['value'],item['repeat'],item['engine']);assert key not in pairs;pairs.add(key)
        report=read(prefix+'/'+item['report']);count+=1
        assert report['passed']==item['passed']
        assert report['engine']==item['engine'] and report['repetition']==item['repeat']
        if not report['passed']:
            failed+=1
            resources=report['resources_after_shutdown']
            assert report.get('error') or report['shutdown_exit_codes']!=[0,0] or report['remaining_server_pids'] or resources['memory_swap_current'] or any(resources['events'].get(k,0) for k in ['oom','oom_kill','oom_group_kill'])
            continue
        passed+=1
        if item['engine']=='rustsql':
            log=contents[prefix+'/'+str(item['value'])+'/rustsql-'+str(item['repeat'])+'-logs/server.log']
            assert log.count(b'cpu affinity: 12 workers on cpus 0-7,12-15')==2
        assert report['shutdown_exit_codes']==[0,0] and not report['remaining_server_pids']
        assert report['limits']['memory.max']==str(24576*2**20) and report['limits']['memory.swap.max']=='0'
        assert report['limits']['cpu.max']=='1200000 100000'
        final=report['resources_after_shutdown']
        assert not any(final['events'].get(k,0) for k in ['oom','oom_kill','oom_group_kill'])
        assert final['memory_swap_current']==0
        settings={n:s for n,s,_ in report['settings']}
        assert settings['fsync']=='on' and settings['synchronous_commit']=='on'
        assert int(settings['max_connections'])==max(100,item['clients']+8)
        expected=updates(item['clients'],meta['warmup_per_client'],meta['bundles_per_client'])
        assert report['state_before_restart']==expected==report['state_after_restart']
        for key in ['measurement']+(['measurement_after_restart'] if meta.get('restart_control') else []):
            m=report[key]
            assert m['passed'] and m['clients']==item['clients'] and m['fact_rows']==item['rows']
            assert m['warmup_per_client']==meta['warmup_per_client'] and m['bundles_per_client']==meta['bundles_per_client']
            assert len(m['warmup'])==item['clients'] and {v['client'] for v in m['warmup']}==set(range(item['clients']))
            assert all(v['passed'] and v['types']==expected_types for v in m['warmup'])
            assert m['sampling_includes_warmup'] and m['sampled_resources']['samples']>0
            for kind,multiplier in statements.items():assert m['latency'][kind]['count']==item['clients']*meta['bundles_per_client']*multiplier
        if meta.get('restart_control'):assert report['state_after_restarted_workload']==expected
    if campaign['kind'] in ('data','connections','restart'):
        engines=['rustsql'] if campaign['kind']=='restart' else list(identities)
        for value in summary['successful_values']:
            assert all((value,repetition,engine) in pairs for repetition in range(1,4) for engine in engines)

result=read('analysis/analysis.json')
assert all(c['complete'] for c in result['campaigns'].values())
assert result['restart_control']['complete']
assert len(result['records'])==sum(len(read(c['prefix']+'/summary.json')['trials']) for c in manifest['campaigns'] if c['kind'] in ('data','connections'))
source_labels={c['source_label']:c['prefix'] for c in manifest['campaigns']}
for record in result['records']:
    label,path=record['source_report'].split('/',1)
    raw=read(source_labels[label]+'/'+path)
    assert raw['passed']==record['passed']
    if not record['passed']:continue
    m=raw['measurement'];sample=m['sampled_resources']
    stages=[raw['resources_before_workload'],m['connected_after_warmup'],m['connected_after_workload']]
    pss=max(sample['sampled_summed_pss_peak_bytes'],*[s['process_sample']['pss_bytes'] if s['process_sample']['complete'] else 0 for s in stages])/2**20
    charged=max(sample['sampled_memory_current_peak_bytes'],*[s['memory_current'] for s in stages])/2**20
    assert record['pss_mib']==pss and record['charged_mib']==charged
    assert record['cpu_ms_per_bundle']==m['server_cpu_us_per_bundle']/1000
    assert record['server_throttled_us']==m['cpu_delta']['throttled_usec']
    assert record['driver_throttled_us']==m['client_cpu_delta']['throttled_usec']
    assert record['performance_timing_eligible']==(not(record['server_throttled_us'] or record['driver_throttled_us']))
restart_campaign=next(c for c in manifest['campaigns'] if c['kind']=='restart')
assert len(result['restart_control']['records'])==len(read(restart_campaign['prefix']+'/summary.json')['trials'])
for record in result['restart_control']['records']:
    label,path=record['source_report'].split('/',1)
    raw=read(source_labels[label]+'/'+path)
    assert raw['passed']==record['passed']
    if not record['passed']:continue
    for phase,key,pre in [('before','measurement','resources_before_workload'),('after','measurement_after_restart','resources_before_restarted_workload')]:
        m=raw[key];sample=m['sampled_resources'];stages=[raw[pre],m['connected_after_warmup'],m['connected_after_workload']]
        pss=max(sample['sampled_summed_pss_peak_bytes'],*[s['process_sample']['pss_bytes'] if s['process_sample']['complete'] else 0 for s in stages])/2**20
        charged=max(sample['sampled_memory_current_peak_bytes'],*[s['memory_current'] for s in stages])/2**20
        assert record[phase+'_pss_mib']==pss and record[phase+'_charged_mib']==charged
        assert record[phase+'_cpu_ms_per_bundle']==m['server_cpu_us_per_bundle']/1000
    for key in ['setup_cache_trace','cache_after_plans','cache_after_workload','cache_immediately_after_restart','cache_after_restart_aggregate','cache_after_restarted_workload']:
        assert record[key]==raw.get(key)
        stats=record[key]
        if key=='setup_cache_trace':continue
        if stats.get('available'):
            assert stats['counters']=={k:int(v) for k,v in (part.split('=',1) for part in stats['raw'].split())}
print(json.dumps({'passed':True,'archive_members':len(contents),'captured_trials':count,'passed_trials':passed,'retained_failed_trials':failed,'sql_rerun':False}))

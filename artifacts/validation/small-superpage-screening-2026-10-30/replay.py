#!/usr/bin/env python3
"""Verify the frozen native PostgreSQL comparison and reproduce its summaries."""
import hashlib,json,statistics,tarfile,re
from pathlib import Path

def verify(root):
 manifest=json.loads((root/'manifest.json').read_text());archive=root/'evidence.tar.gz'
 assert hashlib.sha256(archive.read_bytes()).hexdigest()==manifest['archive_sha256']
 with tarfile.open(archive) as tar:
  entries=tar.getmembers();assert all(e.isfile() for e in entries)
  assert len(entries)==len(manifest['members']) and {e.name for e in entries}==set(manifest['members'])
  data={e.name:tar.extractfile(e).read() for e in entries}
 assert all(hashlib.sha256(data[n]).hexdigest()==sha for n,sha in manifest['members'].items())
 expected=json.loads((root/'results.json').read_text());actual={};statements=0
 for phase in ['capacity','medium']:
  prefix=f'small-superpage-{phase}-20261030';meta=json.loads(data[prefix+'/metadata.json']);summary=json.loads(data[prefix+'/summary.json'])
  assert meta['binary_sha256']['rustsql']=='c01c29f2f75102699fda0d9799b4fc68c3bcad480a90ecadc1d641f32c43ec28'
  assert meta['candidate_policy']==''
  assert meta['binary_sha256']['rustsql-baseline']=='8e5bfc2b093055a5d2257dc817037a2b2f2906faf7c1b83e431f472a2f0513f0'
  assert summary['complete'] and summary['passed'] and len(summary['trials'])==15
  assert all(hashlib.sha256(data[prefix+'-source/'+n].replace(b'\r\n',b'\n')).hexdigest()==sha for n,sha in meta['source_lf_sha256'].items())
  actual[phase]={}
  for engine in ['postgresql','rustsql-baseline','rustsql']:
   reports=[json.loads(data[f'{prefix}/{phase}-{engine}-{i}.json']) for i in range(1,6)]
   for d in reports:
    m=d['measurement'];assert d['passed'] and m['passed'] and d['shutdown_exit_codes']==[0,0] and not d['remaining_server_pids']
    assert d['state_after_restart']==d['state_before_restart']
    settings={n:s for n,s,_ in d['settings']};assert settings['fsync']=='on' and settings['synchronous_commit']=='on'
    assert d['limits']['cpu.max']=='400000 100000' and d['limits']['memory.max']==str(2048*2**20) and d['limits']['memory.swap.max']=='0'
    assert not any(d['resources_after_shutdown']['events'][n] for n in ['oom','oom_kill','oom_group_kill','max'])
    assert not m['cpu_delta']['nr_throttled'] and not m['client_cpu_delta']['nr_throttled'] and not d['resources_after_shutdown']['memory_swap_current']
    count=meta['clients']*meta['bundles'];statements+=count*19
    assert abs(m['server_cpu_us_per_bundle']-m['cpu_delta']['usage_usec']/count)<1e-6
    assert abs(m['bundles_per_second']-count/m['elapsed_seconds'])<1e-6
    assert d['state_after_restart']['committed_updates']==meta['clients']*(meta['bundles']+meta['warmup'])*4
    for kind,n in (meta['statements_per_bundle']|{'bundle':1}).items():
     l=m['latency'][kind];assert l['count']==count*n and 0<=l['p50_ms']<=l['p95_ms']<=l['p99_ms']<=l['max_ms']
    if phase=='medium':assert m['offered_bundles_per_second']==9.4 and m['schedule_lag']['count']==count and m['schedule_lag']['max_ms']<1000*meta['clients']/9.4
   ms=[d['measurement'] for d in reports]
   metrics={'cpu_ms_per_bundle':lambda m:m['server_cpu_us_per_bundle']/1000,'statements_per_second':lambda m:m['bundles_per_second']*19,'server_cpu_percent_of_four_cores':lambda m:m['server_cpu_seconds']/m['elapsed_seconds']/4*100,'client_cpu_percent_of_two_cores':lambda m:m['client_cpu_delta']['usage_usec']/1e6/m['elapsed_seconds']/2*100,'peak_cgroup_mib':lambda m:m['sampled_resources']['sampled_memory_current_peak_bytes']/2**20,'peak_process_pss_mib':lambda m:m['sampled_resources']['sampled_summed_pss_peak_bytes']/2**20}
   if phase=='medium':metrics['schedule_lag_p99_ms']=lambda m:m['schedule_lag']['p99_ms']
   for kind in ['read','update','aggregate','range','join','bundle']:
    for pct in ['p50','p95','p99']:metrics[kind+'_'+pct+'_ms']=lambda m,k=kind,p=pct:m['latency'][k][p+'_ms']
   actual[phase][engine]={k:{'median':statistics.median(f(m) for m in ms),'minimum':min(f(m) for m in ms),'maximum':max(f(m) for m in ms)} for k,f in metrics.items()}
 assert actual==expected and statements==136800

 def compare(before,after):
  def describe(v):return {'values':v,'median':statistics.median(v),'min':min(v),'max':max(v)}
  return {'baseline':describe(before),'candidate':describe(after),'median_change_pct':(statistics.median(after)/statistics.median(before)-1)*100,'paired_change_pct':describe([(y/x-1)*100 for x,y in zip(before,after)])}
 prefix='small-superpage-connection-calibration-20261030'
 meta=json.loads(data[prefix+'/metadata.json']);commands=json.loads(data[prefix+'/commands.json'])
 assert meta['repeats']==5 and meta['memory_mib']==4096 and meta['allocator']=='hearth' and meta['waves']==2 and meta['accepted_per_wave']==800
 assert meta['server_cpus']==[0,2,4,6] and meta['client_cpus']==[8,10]
 assert meta['binaries']['baseline']['sha256']=='8e5bfc2b093055a5d2257dc817037a2b2f2906faf7c1b83e431f472a2f0513f0'
 assert meta['binaries']['candidate']['sha256']=='c01c29f2f75102699fda0d9799b4fc68c3bcad480a90ecadc1d641f32c43ec28'
 assert all(hashlib.sha256(data[prefix+'/source/'+Path(n).name]).hexdigest()==sha for n,sha in meta['source_sha256'].items())
 order=[(label,i) for i in range(1,6) for label in (('baseline','candidate') if i%2 else ('candidate','baseline'))]
 assert [(c['label'],c['repeat']) for c in commands]==order and all(c['exit']==0 and c['passed'] for c in commands)
 reports={}
 for label,i in order:
  d=json.loads(data[f'{prefix}/{label}-{i}.json']);reports[label,i]=d
  assert d['passed'] and not d['remaining_server_pids'] and d['binary_sha256']==meta['binaries'][label]['sha256']
  assert d['allocator']=={'requested':'hearth','live_witnesses':['hearth']}
  assert d['limits']=={'memory.max':str(4096*2**20),'memory.swap.max':'0','cpu.max':'400000 100000','pids.max':'1024'}
  assert d['harness_sha256']==meta['source_sha256']['/home/rustsql/pgrust/scripts/validation/connection_queue_probe.py']
  assert d['wrapper_sha256']==meta['source_sha256']['/home/rustsql/small_pages_queue_probe.py']
  assert len(d['waves'])==2
  for j,w in enumerate(d['waves'],1):
   assert w['wave']==j and w['passed'] and w['accepted']==800 and w['initially_ready']==500 and w['initially_queued']==300
   assert w['overflow_rejection']['sqlstate']=='53300' and len(w['startup_seconds'])==800
   assert all(0<=t<1000 for t in w['startup_seconds'])
   assert w['stats_before']['queued']==300 and w['stats_after']['queued']==0 and w['stats_after']['served']==300*j
   assert not w['stats_after']['timeouts'] and not w['stats_after']['hangups']
   h=w['resources_while_held'];assert 800<=h['pids_current']<=1024 and sum(h['thread_affinity_counts'].values())==h['pids_current']
   assert all(set(map(int,k.split(',')))<=set(meta['server_cpus']) for k in h['thread_affinity_counts'])
  before,after=d['resources_before'],d['resources_after']
  assert all(v==after['cpu'].get(k,0)-before['cpu'].get(k,0) for k,v in d['cpu_delta'].items())
  assert d['cpu_delta']['usage_usec']>0 and d['cpu_delta'].get('throttled_usec',0)==0
  end=d['resources_after_shutdown'];assert not any(end['events'].get(k,0) for k in ('high','max','oom','oom_kill','oom_group_kill'))
  assert end['memory_swap_current']==0 and 0<end['memory_peak']<=4096*2**20
  assert d['sampled_resources']['samples']>0 and d['sampled_resources']['incomplete_process_samples']==0
 expected_connections=json.loads((root/'connection-results.json').read_text())
 assert expected_connections==json.loads(data['small-superpage-connection-comparison.json']) and expected_connections['metadata']==meta
 assert expected_connections['accepted_startups']==16000 and expected_connections['overflow_rejections']==20 and expected_connections['all_pairs_more_cpu']
 getters={'cpu_us_per_accepted_startup':lambda d:d['cpu_delta']['usage_usec']/1600,'first_wave_seconds':lambda d:d['waves'][0]['seconds'],'both_waves_seconds':lambda d:sum(w['seconds'] for w in d['waves']),'peak_cgroup_mib':lambda d:d['resources_after_shutdown']['memory_peak']/2**20,'minor_and_major_faults':lambda d:d['resources_after']['memory']['pgfault']-d['resources_before']['memory']['pgfault']}
 for k,f in getters.items():assert expected_connections[k]==compare(*[[f(reports[label,i]) for i in range(1,6)] for label in ('baseline','candidate')])
 assert expected_connections['cpu_us_per_accepted_startup']['paired_change_pct']['min']>0
 cm=json.loads(data['small-superpage-candidate/manifest.json']);assert not cm['qualified'] and cm['binaries']['postgres']=='c01c29f2f75102699fda0d9799b4fc68c3bcad480a90ecadc1d641f32c43ec28'
 assert hashlib.sha256(data['small-superpage-candidate/candidate.patch']).hexdigest()==cm['runtime_patch_sha256']
 assert all(hashlib.sha256(data['small-superpage-candidate/source/'+n]).hexdigest()==h for n,h in cm['runtime_source_sha256'].items())
 restoration=json.loads(data['small-superpage-candidate/restoration.json'])
 assert restoration['source_head']==cm['source_head'] and restoration['windows_and_linux_verified'] and len(restoration['restored_files'])==7
 assert restoration['target_release_sha256']=='8e5bfc2b093055a5d2257dc817037a2b2f2906faf7c1b83e431f472a2f0513f0'
 assert json.loads(data['small-superpage-candidate/decision.json'])['decision']=='rejected; no runtime or default promotion'
 log=data['small-superpage-candidate/small-superpage-tests.log'].decode();counts=re.findall(r'test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;',log)
 assert tuple(sum(int(v[i]) for v in counts) for i in range(3))==(117,0,1)
 d=json.loads(data['small-superpage-candidate-correctness.json']);assert d['passed'] and d['engines']['postgresql']['result']==d['engines']['rustsql']['result']
 assert d['engines']['rustsql']['binary_sha256']=='c01c29f2f75102699fda0d9799b4fc68c3bcad480a90ecadc1d641f32c43ec28'
 assert all(e['shutdown_exit_codes']==[0] and len(e['result']['checks'])==8 and e['result']['committed_updates']==192 for e in d['engines'].values())
 for folder in ['small-superpage-profile-symbols-candidate','point-profile-symbols-candidate']:
  sm=json.loads(data[folder+'/manifest.json']);assert sm['same_text'] and sm['before_sections']['.text']==sm['after_sections']['.text']
 assert not json.loads(data['small-superpage-connection-profile-20261030/provenance.json'])['valid_capture']
 for label in ['small-superpage-connection-profile-v2-20261030','retained-connection-profile-20261030']:
  d=json.loads(data[label+'.json']);assert d['passed'] and d['profiles'][0]['exit'] in (0,-2,130) and len(d['profiles'][0]['server_pids'])>=2
  assert 'Total Lost Samples: 0' in data[label+'/perf-self-symbols.txt'].decode()
 diagnostic=json.loads(data['small-superpage-profile-analysis.json']);assert diagnostic['candidate_samples']==251 and diagnostic['retained_samples']==21 and not diagnostic['runtime_retained']

 return {'passed':True,'trials':30,'connection_trials':10,'accepted_startups':16000,'timed_statements':statements,'hashed_members':len(data),'production_qualified':False}

if __name__=='__main__':
 if not __debug__:raise SystemExit('assertions required')
 print(json.dumps(verify(Path(__file__).resolve().parent)))

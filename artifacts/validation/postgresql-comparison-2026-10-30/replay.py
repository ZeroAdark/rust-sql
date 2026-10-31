#!/usr/bin/env python3
"""Verify the frozen native PostgreSQL comparison and reproduce its summaries."""
import hashlib,json,statistics,tarfile
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
  prefix=f'pg-compare-{phase}-20261030';meta=json.loads(data[prefix+'/metadata.json']);summary=json.loads(data[prefix+'/summary.json'])
  assert summary['complete'] and summary['passed'] and len(summary['trials'])==10
  assert all(hashlib.sha256(data[prefix+'-source/'+n].replace(b'\r\n',b'\n')).hexdigest()==sha for n,sha in meta['source_lf_sha256'].items())
  actual[phase]={}
  for engine in ['postgresql','rustsql']:
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
 assert actual==expected and statements==91200
 return {'passed':True,'trials':20,'timed_statements':statements,'hashed_members':len(data),'production_qualified':False}

if __name__=='__main__':
 if not __debug__:raise SystemExit('assertions required')
 print(json.dumps(verify(Path(__file__).resolve().parent)))

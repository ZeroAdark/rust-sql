#!/usr/bin/env python3
"""Verify the retained configuration screens without running a database."""
from pathlib import Path
import hashlib,json,tarfile,statistics

def verify(root):
 manifest=json.loads((root/'manifest.json').read_text());archive=root/'evidence.tar.gz'
 assert hashlib.sha256(archive.read_bytes()).hexdigest()==manifest['archive_sha256']
 with tarfile.open(archive) as tar:
  entries=tar.getmembers();assert all(e.isfile() for e in entries)
  assert len(entries)==len(manifest['members']) and {e.name for e in entries}==set(manifest['members'])
  data={e.name:tar.extractfile(e).read() for e in entries}
 assert all(hashlib.sha256(data[n]).hexdigest()==h for n,h in manifest['members'].items())
 actual={};count=0
 for label,phase,repeats in [('pg-serial-pilot-20261030','pilot',1),('pg-serial-medium-pilot-20261030','medium',3),('pg-zero-reserve-medium-20261030','medium',3)]:
  meta=json.loads(data[label+'/metadata.json']);summary=json.loads(data[label+'/summary.json'])
  assert summary['complete'] and summary['passed'] and len(summary['trials'])==repeats*2
  assert meta['binary_sha256']['rustsql']=='8e5bfc2b093055a5d2257dc817037a2b2f2906faf7c1b83e431f472a2f0513f0'
  assert all(hashlib.sha256(data[label+'-source/'+n].replace(b'\r\n',b'\n')).hexdigest()==h for n,h in meta['source_lf_sha256'].items())
  actual[label]={}
  for engine in ['postgresql','rustsql']:
   ms=[]
   for i in range(1,repeats+1):
    d=json.loads(data[f'{label}/{phase}-{engine}-{i}.json']);m=d['measurement'];count+=1
    assert d['passed'] and m['passed'] and d['shutdown_exit_codes']==[0,0] and not d['remaining_server_pids']
    assert d['state_before_restart']==d['state_after_restart']
    assert not any(d['resources_after_shutdown']['events'][n] for n in ['oom','oom_kill','oom_group_kill','max'])
    assert not m['cpu_delta']['nr_throttled'] and not m['client_cpu_delta']['nr_throttled'] and not d['resources_after_shutdown']['memory_swap_current']
    settings={n:s for n,s,_ in d['settings']};assert settings['fsync']=='on' and settings['synchronous_commit']=='on'
    assert settings['max_parallel_workers_per_gather']==('0' if engine=='rustsql' and 'serial' in label else '2')
    assert d['state_after_restart']['committed_updates']==meta['clients']*(meta['bundles']+meta['warmup'])*4
    for kind,n in (meta['statements_per_bundle']|{'bundle':1}).items():assert m['latency'][kind]['count']==meta['clients']*meta['bundles']*n
    ms.append(m)
   actual[label][engine]={'trials':len(ms),'cpu_ms_per_bundle':statistics.median(m['server_cpu_us_per_bundle']/1000 for m in ms),'charged_mib':statistics.median(m['sampled_resources']['sampled_memory_current_peak_bytes']/2**20 for m in ms),'pss_mib':statistics.median(m['sampled_resources']['sampled_summed_pss_peak_bytes']/2**20 for m in ms),'p99_ms':{k:statistics.median(m['latency'][k]['p99_ms'] for m in ms) for k in ['read','update','aggregate','range','join','bundle']}}
 assert actual==json.loads((root/'results.json').read_text()) and count==14
 return {'passed':True,'trials':count,'hashed_members':len(data),'production_qualified':False}

if __name__=='__main__':
 if not __debug__:raise SystemExit('assertions required')
 print(json.dumps(verify(Path(__file__).resolve().parent)))

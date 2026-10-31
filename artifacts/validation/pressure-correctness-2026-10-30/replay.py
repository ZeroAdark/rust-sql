#!/usr/bin/env python3
"""Recheck archived pressure reports and hashes without starting a database."""
import hashlib
import json
import math
from pathlib import Path
import tarfile

ROOT = Path(__file__).resolve().parent

def check(ok, message):
    if not ok:
        raise ValueError(message)

def oracle(rows):
    sort, top, agg = hashlib.sha256(), hashlib.sha256(), hashlib.sha256()
    seen = 0
    for group in range(96, -1, -1):
        for i in range(rows - (rows - group) % 97, 0, -97):
            row = f'{i}\t{group}\t{i * 17 % 1000}\t{str(i).zfill(8) * 16}\n'.encode()
            sort.update(row)
            if seen < 32:
                top.update(row)
            seen += 1
    check(seen == rows, 'oracle cardinality')
    for i in range(1, rows + 1):
        agg.update(f'{i}\t2\t{2 * (i * 17 % 1000)}\n'.encode())
    return {'sort': [rows, sort.hexdigest()], 'topk': [32, top.hexdigest()], 'aggregate': [rows, agg.hexdigest()]}

def resources(r, cap):
    check(r['memory_peak'] <= cap and r['memory_swap_current'] == 0, 'memory bounds')
    check(all(r['events'][k] == 0 for k in ('oom','oom_kill','oom_group_kill')), 'OOM event')
    check(r['cpu']['nr_throttled'] == 0 and r['cpu']['throttled_usec'] == 0, 'CPU throttling')

def churn(r):
    check(r['passed'] and r['restart_verified'], 'churn result/restart')
    check(r['durability'] == {'fsync':'on','synchronous_commit':'on','max_active_queries':'3'}, 'durability/admission')
    check(r['shutdown_exit_codes'] == [0,0] and not r['remaining_server_pids'], 'churn shutdown')
    cap=r['parameters']['memory_mib']*1048576
    check(r['limits'] == {'memory.max':str(cap),'memory.swap.max':'0','cpu.max':'400000 100000','pids.max':'1024'}, 'churn limits')
    check(len(r['waves']) == r['parameters']['waves'] == 100, 'wave count')
    prior=[0,0,0]
    for index,w in enumerate(r['waves'],1):
        check(w['passed'] and w['wave']==index and w['accepted']==800 and w['initially_ready']==500 and w['initially_queued']==300,'wave counts')
        check(w['overflow_rejection']['sqlstate']=='53300' and not w['overflow_rejection']['ready'],'overflow')
        before,after=w['stats_before'],w['stats_after']
        check(before['queued']==300 and after['queued']==0 and after['served']==before['served']+300,'queue drain')
        check(after['total_queued']==before['total_queued']==after['served'],'queue accounting')
        check(not any(x[k] for x in (before,after) for k in ('timeouts','hangups')),'lost queued startup')
        check(len(w['startup_seconds'])==800 and all(math.isfinite(x) and 0 <= x < 45 for x in w['startup_seconds']),'startup timings')
        check(all(b>=a for a,b in zip(prior,w['sql_progress'])),'SQL progress counters')
        prior=w['sql_progress']
        for snap in (w['resources_while_held'],w['after_drain']): resources(snap,cap)
        for mask in w['resources_while_held']['thread_affinity_counts']:
            check(set(map(int,mask.split(','))) <= {0,2,4,6},'thread affinity')
    check(len(r['sql_progress_windows'])==20,'progress window count')
    for index,w in enumerate(r['sql_progress_windows'],1):
        check(w['ending_wave']==index*5 and all(b>a for a,b in zip(w['before'],w['after'])),'SQL worker stalled')
    check(r['committed_accounts']==[[1,r['sql_operations'][0]],[2,r['sql_operations'][1]]],'committed balances')
    check(all(b>=a for a,b in zip(prior,r['sql_operations'])),'final counters')
    check(len(r['settle'])==4 and r['settle'][-1]['seconds']>=15,'settling')
    resources(r['resources_after_shutdown'],cap)
    return {'accepted':80000,'transactions':sum(r['sql_operations'][:2]),'analytics':r['sql_operations'][2],
            'peak_mib':r['resources_after_shutdown']['memory_peak']/1048576,'settled_mib':r['settle'][-1]['resources']['memory_current']/1048576}

def spill(r, manifest):
    check(r['passed'] and r['rows'] in (500000,1000000),'spill pass/rows')
    expected=oracle(r['rows']);check(r['oracle']==expected,'oracle hashes')
    for name,server in r['servers'].items():
        check(server['binary_sha256']==manifest[name+'_sha256'],'spill binary')
        check(server['passed'] and server['shutdown_exit_codes']==[0] and not server['remaining_server_pids'],'spill shutdown/pass')
        check(server['durability']==['on','on'],'spill durability')
        check(server['limits']=={'memory.max':str(1024**3),'memory.swap.max':'0','cpu.max':'400000 100000','pids.max':'1024'},'spill limits')
        cases=server['cases'];check(len(cases)==6,'spill case count')
        for case,(kind,mem,parallel) in zip(cases,[('sort','64kB',0),('sort','16MB',2),('aggregate','64kB',0),('aggregate','64kB',2),('topk','64kB',0),('topk','64kB',2)]):
            check((case['kind'],case['work_mem'],case['parallel_limit'])==(kind,mem,parallel),'case parameters')
            check(case['passed'] and [case['result']['rows'],case['result']['sha256']]==expected[kind],'SQL result')
            check(not case['remaining_temp_files'],'temporary file leak')
            if kind!='topk' and mem=='64kB': check(sum(f['bytes'] for f in case['temp_files'])>0,'missing physical spill')
            if kind=='topk':check(not case['temp_files'],'unexpected top-k spill in retained evidence')
        check(server['temp_limit_errors']==[{'parallel_limit':p,'sqlstate':'53400'} for p in (0,2)],'temp limit errors')
        check(len(server['cancellations'])==2,'cancellation count')
        for c,p in zip(server['cancellations'],(0,2)):
            check(c['parallel_limit']==p and c['sqlstate']=='57014' and c['live_spill_files'],'live-spill cancellation')
        resources(server['resources_after_shutdown'],1024**3)
    return {'rows':r['rows'],'result_comparisons':12,'limit_errors':4,'live_spill_cancellations':4}

def main():
    manifest=json.loads((ROOT/'manifest.json').read_text()); archive=ROOT/manifest['archive']['name']
    check(archive.stat().st_size==manifest['archive']['bytes'] and hashlib.sha256(archive.read_bytes()).hexdigest()==manifest['archive']['sha256'],'archive hash')
    data={}
    with tarfile.open(archive,'r:gz') as tar:
        for entry in tar:
            check(entry.isfile() and entry.name in manifest['files'] and entry.name not in data,'archive member')
            value=tar.extractfile(entry).read();meta=manifest['files'][entry.name]
            check(len(value)==meta['bytes'] and hashlib.sha256(value).hexdigest()==meta['sha256'],'member hash '+entry.name)
            data[entry.name]=value
    check(set(data)==set(manifest['files']),'missing archive member')
    results={}
    for run in manifest['runs']:
        name=run['name'];r=json.loads(data[name+'/report.json'])
        check(r['passed']==run['passed'] and not r['production_qualified'] and not r['performance_comparison'],'report verdict/scope')
        check(json.loads(data[name+'/command.json'])['exit']==(0 if r['passed'] else 1),'runner status')
        for source,digest in r['source_lf_sha256'].items():
            check(hashlib.sha256(data[name+'/source/'+source].replace(b'\r\n',b'\n')).hexdigest()==digest,'source hash '+source)
        if name.startswith('churn-'):
            check(r['binary_sha256']==manifest['candidate_sha256'],'churn binary');results[name]=churn(r)
        elif run['status']=='pass':results[name]=spill(r,manifest)
        elif run['status']=='failure':
            check(name=='spill-million' and not r['servers']['candidate']['passed'] and r['servers']['reference']['passed'],'retained failure')
            check('memory_cancel_threshold' in r['servers']['candidate']['error'],'watchdog failure reason')
            check(r['servers']['candidate']['shutdown_exit_codes']==[0] and not r['servers']['candidate']['remaining_server_pids'],'failure cleanup')
            resources(r['servers']['candidate']['resources_after_shutdown'],1024**3)
            results[name]={'passed':False,'finding':'memory watchdog canceled a one-million-row sort under 1 GiB'}
    print(json.dumps({'verified_files':len(data),'production_qualified':False,'results':results},indent=2))

if __name__=='__main__':main()

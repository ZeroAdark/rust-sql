"""Validate and summarize the fixed two-wave connection calibration; no production qualification."""
import argparse,hashlib,json,math,statistics
from pathlib import Path
if not __debug__:raise RuntimeError('Verification requires enabled assertions')

def require(ok,message):
    if not ok:raise ValueError(message)
def compare(b,c):
    describe=lambda v:{'values':v,'median':statistics.median(v),'min':min(v),'max':max(v)}
    return {'baseline':describe(b),'candidate':describe(c),'median_change_pct':(statistics.median(c)/statistics.median(b)-1)*100,'paired_change_pct':describe([(y/x-1)*100 for x,y in zip(b,c)])}
def validate(r,m,label):
    required_cpu={'usage_usec','nr_throttled','throttled_usec'}
    require(len(r['parent_cpu_delta'])==1 and all(required_cpu<=d.keys() for d in r['parent_cpu_delta']),'missing parent CPU counters')
    require(all(required_cpu<=r[k]['cpu'].keys() for k in ('resources_before','resources_after','resources_after_shutdown')),'missing server CPU counters')
    before_cpu=r['resources_before']['cpu'];end_cpu=r['resources_after_shutdown']['cpu']
    require(end_cpu['usage_usec']>=r['resources_after']['cpu']['usage_usec']>=before_cpu['usage_usec'],'nonmonotonic CPU')
    require(all(end_cpu[k]==before_cpu[k] for k in ('nr_throttled','throttled_usec')),'server throttled during teardown')
    require({'high','max','oom','oom_kill','oom_group_kill'}<=r['resources_after_shutdown']['events'].keys(),'missing memory event counters')
    require(m['cpu_binding']=='private kernel CPU sets; no CFS quota','CPU-set design')
    require(r['cpu_binding']==[{'cpuset.cpus':'0,2,4,6','cpuset.cpus.effective':'0,2,4,6','cpu.max':'max 100000'}],'CPU set witness')
    require(r['shutdown_exit_codes']==[0] and r['live_environments']==[['PGRUST_ALLOCATOR=hearth']],'exit/environment witness')
    require(len(r['driver_cpu_delta'])==1 and all(d['nr_throttled']==0 and d['throttled_usec']==0 for d in r['driver_cpu_delta']),'driver throttle')
    require(len(r['parent_cpu_delta'])==1 and all(d.get('nr_throttled',0)==0 and d.get('throttled_usec',0)==0 for d in r['parent_cpu_delta']),'parent CPU throttled')
    require(r['cpu_delta'].get('nr_throttled',0)==0,'server CPU throttled')
    require(r['passed'] and not r['remaining_server_pids'],'failed/unclean trial')
    require(r['binary_sha256']==m['binaries'][label]['sha256'],'wrong binary')
    require(r['allocator']=={'requested':'hearth','live_witnesses':['hearth']},'allocator witness')
    require(r['limits']=={'memory.max':str(m['memory_mib']*1048576),'memory.swap.max':'0','cpu.max':'max 100000','pids.max':'1024'},'limits')
    require(r['harness_sha256']==m['source_sha256']['connection_queue_probe.py'],'probe source')
    require(r['wrapper_sha256']==m['source_sha256']['guc_queue_probe.py'],'wrapper source')
    require(len(r['waves'])==2,'wave count')
    for i,w in enumerate(r['waves'],1):
        require(w['wave']==i and w['passed'] and w['accepted']==800 and w['initially_ready']==500 and w['initially_queued']==300,'startup counts')
        require(w['overflow_rejection']['sqlstate']=='53300','overflow result')
        require(w['stats_before']['queued']==300 and w['stats_after']['queued']==0 and w['stats_after']['served']==300*i,'queue drain')
        require(w['stats_after']['timeouts']==0 and w['stats_after']['hangups']==0,'lost startup')
        require(len(w['startup_seconds'])==800 and all(math.isfinite(x) and x>=0 for x in w['startup_seconds']),'startup timing count')
        held=w['resources_while_held'];require(800<=held['pids_current']<=1024,'held task count')
        require(sum(held['thread_affinity_counts'].values())==held['pids_current'],'affinity task count')
        require(all(set(map(int,k.split(',')))<=set(m['server_cpus']) for k in held['thread_affinity_counts']),'server affinity')
    before,after=r['resources_before'],r['resources_after']
    for k,v in r['cpu_delta'].items():require(v==after['cpu'].get(k,0)-before['cpu'].get(k,0),'CPU delta')
    require(r['cpu_delta']['usage_usec']>0 and r['cpu_delta'].get('throttled_usec',0)==0,'CPU missing/throttled')
    end=r['resources_after_shutdown'];require(not any(end['events'].get(k,0) for k in ('high','max','oom','oom_kill','oom_group_kill')),'memory pressure event')
    require(end['memory_swap_current']==0 and 0<end['memory_peak']<=m['memory_mib']*1048576,'swap/peak')
    require(r['sampled_resources']['samples']>0 and r['sampled_resources']['incomplete_process_samples']==0,'resource sampling')
def analyze(root):
    m=json.loads((root/'metadata.json').read_text())
    require(m['repeats']==5 and m['memory_mib']==4096 and m['allocator']=='hearth' and m['waves']==2 and m['accepted_per_wave']==800,'design')
    require(m['server_cpus']==[0,2,4,6] and m['client_cpus']==[8,10],'CPU design')
    require({k:v['sha256'] for k,v in m['binaries'].items()}=={'baseline':'02cfbca6869bb352c8081b1fb275cdd4317b33a72b8feb05c563408e4c48d454','candidate':'57baa2ef25bdfebccd3bf851fb84692828a72e2b8541faddfe1ace0a281cabe6'},'binary identities')
    preflight=json.loads((root/'cpuset-preflight.json').read_text())
    proof=json.loads(preflight['stdout'])
    require(preflight['exit']==0 and proof['passed'] and proof['driver_allowed']==[8,10] and proof['server']=={'allowed':[0,2,4,6],'escape_rejected':True},'CPU set escape proof')
    require(proof['server_cpu_max']==proof['parent_cpu_max']=='max 100000','CPU quota proof')
    for p,sha in m['source_sha256'].items():require(hashlib.sha256((root/'source'/Path(p).name).read_bytes()).hexdigest()==sha,'source hash: '+p)
    commands=json.loads((root/'commands.json').read_text())
    expected=[(label,i) for i in range(1,6) for label in (('baseline','candidate') if i%2 else ('candidate','baseline'))]
    require([(c['label'],c['repeat']) for c in commands]==expected,'interleaving/order/completeness')
    require(all(c['exit']==0 and c['passed'] for c in commands),'command failed')
    reports={}
    for label,i in expected:
        r=json.loads((root/f'{label}-{i}.json').read_text());validate(r,m,label);reports[label,i]=r
    def metric(f):return compare(*[[f(reports[label,i]) for i in range(1,6)] for label in ('baseline','candidate')])
    cpu=metric(lambda r:r['cpu_delta']['usage_usec']/1600)
    return {'evidence_valid':True,'production_qualified':False,'scope':'4 GiB two-wave startup calibration, five interleaved pairs','accepted_startups':16000,'overflow_rejections':20,'cpu_us_per_accepted_startup':cpu,'cpu_us_per_accepted_startup_including_shutdown':metric(lambda r:(r['resources_after_shutdown']['cpu']['usage_usec']-r['resources_before']['cpu']['usage_usec'])/1600),'first_wave_seconds':metric(lambda r:r['waves'][0]['seconds']),'both_waves_seconds':metric(lambda r:sum(w['seconds'] for w in r['waves'])),'peak_cgroup_mib':metric(lambda r:r['resources_after_shutdown']['memory_peak']/1048576),'minor_and_major_faults':metric(lambda r:r['resources_after']['memory']['pgfault']-r['resources_before']['memory']['pgfault']),'all_pairs_more_cpu':cpu['paired_change_pct']['min']>0,'metadata':m,'limitations':['Wave durations include client orchestration and diagnostic snapshots; they are not standalone authentication latency.','Startup-phase CPU covers the server cgroup between the before/after snapshots. The including-shutdown metric extends the same start to clean server shutdown so delayed cleanup is not hidden.','Short WSL x86-64 calibration; not a representative Linux-server qualification.']}
if __name__=='__main__':
    p=argparse.ArgumentParser();p.add_argument('--input-dir',type=Path,required=True);p.add_argument('--output',type=Path,required=True);a=p.parse_args()
    result=analyze(a.input_dir);a.output.write_text(json.dumps(result,indent=2)+'\n');print(json.dumps({k:result[k] for k in ('accepted_startups','all_pairs_more_cpu')}));print(json.dumps({k:result[k]['median_change_pct'] for k in ('cpu_us_per_accepted_startup','first_wave_seconds','peak_cgroup_mib')}))

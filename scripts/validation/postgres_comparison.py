#!/usr/bin/env python3
"""Compare fixed mixed SQL work with native PostgreSQL in private Linux cgroups."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import resource
import statistics
import time
import psycopg
from connection_churn_probe import CheckedCluster
from fixed_workload import PER_BUNDLE, SQL, exercise, grouped, setup, verify_state
from resource_scope import DelegatedScope, snapshot


def trial(args, scope, label, repeat, rate, phase):
    reference = label == 'postgresql'
    binaries = args.reference if reference else args.candidate
    group, launcher = scope.server(args.memory_mib)
    settings = ['autovacuum=off', 'work_mem=16MB', 'shared_buffers=128MB', 'max_parallel_workers_per_gather=2']
    if not reference:
        settings.append('max_active_queries=' + str(args.clients))
    cluster = CheckedCluster(binaries, args.work_dir, reference=reference, launcher=launcher,
                             allocator=None if reference else 'hearth', settings=settings)
    if not reference and args.candidate_policy:
        cluster.env['PGRUST_MEMPOLICY'] = args.candidate_policy
    report = {'passed':False,'engine':label,'phase':phase,'repetition':repeat,'cluster':str(cluster.root),
              'limits':{n:(group/n).read_text().strip() for n in ('cpu.max','memory.max','memory.swap.max','pids.max')}}
    try:
        with cluster:
            report['version'] = cluster.version
            assert cluster.process.pid in set(map(int,(group/'cgroup.procs').read_text().split()))
            assert os.sched_getaffinity(cluster.process.pid) == scope.server_cpus
            with psycopg.connect(cluster.dsn,autocommit=True) as connection:
                setup(connection,args.rows,args.clients,reference=reference)
                report['settings']=connection.execute("SELECT name,setting,unit FROM pg_settings WHERE name IN ('fsync','synchronous_commit','work_mem','shared_buffers','max_parallel_workers_per_gather','max_connections','jit') ORDER BY name").fetchall()
                effective={n:s for n,s,_ in report['settings']}
                assert effective['fsync']=='on' and effective['synchronous_commit']=='on'
                report['relation_sizes']=connection.execute("SELECT c.relname,am.amname,pg_relation_size(c.oid),pg_total_relation_size(c.oid) FROM pg_class c JOIN pg_am am ON c.relam=am.oid WHERE c.relname IN ('bench_accounts','bench_facts','bench_dim') ORDER BY c.relname").fetchall()
                report['plans']={kind:connection.execute('EXPLAIN (FORMAT JSON) '+SQL[kind],(1,4096) if kind=='range' else None).fetchone()[0] for kind in ('aggregate','range','join')}
            report['measurement']=exercise(cluster.dsn,group,args.rows,args.clients,args.bundles,args.warmup,bundle_rate=rate)
            with psycopg.connect(cluster.dsn,autocommit=True) as connection:
                report['state_before_restart']=verify_state(connection,args.clients,args.bundles+args.warmup)
                assert connection.execute(SQL['aggregate']).fetchall()==grouped(1,args.rows,128)
            cluster.stop();cluster.start()
            with psycopg.connect(cluster.dsn,autocommit=True) as connection:
                report['state_after_restart']=verify_state(connection,args.clients,args.bundles+args.warmup)
                assert report['state_after_restart']==report['state_before_restart']
                assert connection.execute(SQL['aggregate']).fetchall()==grouped(1,args.rows,128)
        report['passed']=True
    except Exception as error:
        report['error']=repr(error)
    finally:
        report['shutdown_exit_codes']=cluster.shutdown_exit_codes
        report['resources_after_shutdown']=snapshot(group)
        report['remaining_server_pids']=(group/'cgroup.procs').read_text().split()
        if report['shutdown_exit_codes']!=[0,0] or report['remaining_server_pids']:report['passed']=False
        if any(report['resources_after_shutdown']['events'][k] for k in ('oom','oom_kill','oom_group_kill')):report['passed']=False
        if report['resources_after_shutdown']['memory_swap_current']:report['passed']=False
        if not report['remaining_server_pids']:group.rmdir()
    output=args.output_dir/f'{phase}-{label}-{repeat}.json';output.write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps({'file':output.name,'passed':report['passed'],'error':report.get('error'),'measurement':{k:report.get('measurement',{}).get(k) for k in ['bundles_per_second','server_cpu_us_per_bundle','elapsed_seconds']}}),flush=True)
    if not report['passed']:raise RuntimeError('comparison failed; retained '+str(output))
    return report


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    for name in ('candidate','reference','work-dir','output-dir'):parser.add_argument('--'+name,type=Path,required=True)
    parser.add_argument('--rows',type=int,default=1000000)
    parser.add_argument('--clients',type=int,default=6)
    parser.add_argument('--memory-mib',type=int,default=2048)
    parser.add_argument('--bundles',type=int,default=100)
    parser.add_argument('--warmup',type=int,default=20)
    parser.add_argument('--repeats',type=int,default=5)
    parser.add_argument('--bundle-rate',type=float)
    parser.add_argument('--candidate-policy',default='')
    parser.add_argument('--phase',default='capacity',choices=['capacity','medium','pilot'])
    args=parser.parse_args()
    if not __debug__ or not 4096<=args.rows<=1000000 or not 1<=args.clients<=6 or not 1<=args.bundles<=400 or not 1<=args.repeats<=5 or not 1<=args.warmup<=50:parser.error('bounded parameters and assertions required')
    if (args.phase=='medium') != (args.bundle_rate is not None):parser.error('medium phase requires an explicit positive bundle rate')
    if args.bundle_rate is not None and not 0<args.bundle_rate<=1000:parser.error('positive rate up to 1000 required')
    args.output_dir.mkdir(parents=True,exist_ok=False)
    resource.setrlimit(resource.RLIMIT_STACK,(65520*1024,resource.getrlimit(resource.RLIMIT_STACK)[1]))
    scope=DelegatedScope([0,2,4,6],[8,10])
    metadata={'platform':platform.platform(),'python':platform.python_version(),'cpu_model':next(x.split(':',1)[1].strip() for x in Path('/proc/cpuinfo').read_text().splitlines() if x.startswith('model name')),
              'candidate_policy':args.candidate_policy,'phase':args.phase,'rows':args.rows,'clients':args.clients,'memory_mib':args.memory_mib,'bundles':args.bundles,'warmup':args.warmup,'repeats':args.repeats,'bundle_rate':args.bundle_rate,'server_cpus':[0,2,4,6],'client_cpus':[8,10],'statements_per_bundle':PER_BUNDLE,
              'binary_sha256':{n:hashlib.sha256((p/'postgres').read_bytes()).hexdigest() for n,p in [('postgresql',args.reference),('rustsql',args.candidate)]},
              'source_lf_sha256':{n:hashlib.sha256(Path(__file__).with_name(n).read_bytes().replace(b'\r\n',b'\n')).hexdigest() for n in ['postgres_comparison.py','fixed_workload.py','cluster.py','connection_churn_probe.py','connection_queue_probe.py','resource_scope.py']}}
    (args.output_dir/'metadata.json').write_text(json.dumps(metadata,indent=2)+'\n')
    trials=[]
    for repetition in range(1,args.repeats+1):
        for label in (('postgresql','rustsql') if repetition%2 else ('rustsql','postgresql')):
            r=trial(args,scope,label,repetition,args.bundle_rate,args.phase);trials.append({'engine':label,'repetition':repetition,'passed':r['passed']})
            (args.output_dir/'summary.json').write_text(json.dumps({'complete':False,'trials':trials},indent=2)+'\n')
    (args.output_dir/'summary.json').write_text(json.dumps({'complete':True,'passed':True,'production_qualified':False,'trials':trials},indent=2)+'\n')

if __name__=='__main__':main()

from pathlib import Path, PurePosixPath
import hashlib, io, json, statistics, tarfile

if not __debug__:raise RuntimeError('Verification requires enabled assertions')
CAMPAIGNS={
 'memory-localization-pilot-20261031':('memory-localization-source-20261031',[10000],['idle','read','update','range','aggregate','join'],['postgresql','rustsql'],1),
 'memory-localization-20261031':('memory-localization-source-20261031',[10000,1000000],['idle','read','update','range','aggregate','join'],['postgresql','rustsql'],2),
 'memory-ownership-20261031':('memory-ownership-source-20261031',[1000000],['idle','aggregate','join'],['rustsql'],2),
 'memory-startup-stable-20261031':('memory-startup-stable-source-20261031',[10000,1000000],['idle'],['postgresql','rustsql'],3),
}
HASHES={'postgresql':'07b16e1aa4f0e2206c37b339b6e4cc813aa8ab9e3c5516334f753b3d90d0c536','rustsql':'02cfbca6869bb352c8081b1fb275cdd4317b33a72b8feb05c563408e4c48d454'}
STAGES=['before_connections','connected','after_first','after_repeated','after_disconnect','after_idle_one_second']

def review(data):
    result={'production_qualified':False,'performance_qualified':False,'campaigns':{},'stage_pss_mib':[],'observer_pss_change_mib':[],'excluded_observations':[]}
    for label,(source,sizes,kinds,engines,repeats) in CAMPAIGNS.items():
        read=lambda name:json.loads(data[label+'/'+name])
        meta=read('metadata.json');summary=read('summary.json')
        assert meta['binary_sha256']==HASHES and meta['assertions_enabled']
        assert meta['clients']==6 and meta['rows']==sizes and meta['kinds']==kinds and meta['repeats']==repeats
        assert meta['server_cpus']==[0,2,4,6] and meta['client_cpus']==[8,10]
        for name,sha in meta['source_sha256'].items():assert hashlib.sha256(data[source+'/'+name]).hexdigest()==sha
        expected={f'{n}-{kind}-{engine}-{r}.json' for n in sizes for kind in kinds for engine in engines for r in range(1,repeats+1)}
        assert summary['complete'] and not summary['production_qualified']
        assert len(summary['trials'])==len(expected) and {t['report'] for t in summary['trials']}==expected
        assert all(t['passed'] and t['error'] is None for t in summary['trials'])
        points={}
        for name in sorted(expected):
            trial=read(name);m=trial['measurement'];kind=trial['kind'];engine=trial['engine'];n=trial['rows']
            assert name==f"{n}-{kind}-{engine}-{trial['repeat']}.json" and trial['passed']
            assert trial['shutdown_exit_codes']==[0,0,0] and not trial['remaining_pids']
            assert trial['live_environment']==([] if engine=='postgresql' else ['PGRUST_ALLOCATOR=hearth'])
            assert trial['limits']=={'memory.max':'2147483648','memory.swap.max':'0','cpu.max':'max 100000' if 'ownership' in label else '400000 100000'}
            final=trial['final_resources']
            assert final['memory_swap_current']==0 and all(final['events'][k]==0 for k in ('max','oom','oom_kill','oom_group_kill'))
            count=0 if kind=='idle' else 32 if kind in ('read','update') else 12
            types=None if kind=='idle' else [23,20] if kind in ('read','update') else [20,20] if kind=='range' else [23,20,20]
            assert m['queries']==6*count
            for field,stage in [('connections','connected'),('first_results','first'),('repeated_results','repeated')]:
                rows=m[field];assert len(rows)==6 and {r['client'] for r in rows}==set(range(6))
                assert all(r['passed'] and r['stage']==stage for r in rows)
                if field!='connections':assert all(r['types']==types for r in rows)
                if field=='repeated_results':assert all(r['queries']==count for r in rows)
            updated=sorted((client*20000+(i*67)%20000+1,1) for client in range(6) for i in range(32)) if kind=='update' else []
            assert trial['committed_updates']==len(updated)
            assert trial['state_sha256']==hashlib.sha256(json.dumps(updated).encode()).hexdigest()
            if 'startup-stable' in label:
                trace=trial['startup_observation_trace']
                assert 2<=len(trace)<=50 and trace[-1]['stable']
                assert trace[-1]['complete'] and trace[-2]['complete']
                assert trace[-1]['pids_before']==trace[-1]['pids_after']==trace[-2]['pids_before']==trace[-2]['pids_after']
            for stage in STAGES:
                o=trial[stage] if stage in trial else m[stage]
                if not o['process_sample']['complete'] or not o['mapping_summary']['complete']:
                    assert label in ('memory-localization-pilot-20261031','memory-localization-20261031') and engine=='postgresql' and stage=='before_connections'
                    assert not o['process_sample']['complete'] and o['mapping_summary']['complete']
                    result['excluded_observations'].append({'campaign':label,'report':name,'stage':stage,'reason':'incomplete process sample'})
                    continue
                assert o['process_sample']['pss_bytes']>0 and o['process_sample']['process_count']>0
                points.setdefault((n,kind,engine,stage),[]).append(o['process_sample']['pss_bytes']/2**20)
                if 'ownership' in label:
                    after=o['after_diagnostics'];assert after['process_sample']['complete'] and after['mapping_summary']['complete']
                    for function in ('pgrust_memory_accounts','pgrust_memory_owner_end','pgrust_memory_owners'):
                        table=o['diagnostics'][function]
                        assert table['columns'] and table['rows'] and all(len(row)==len(table['columns']) for row in table['rows'])
                    result['observer_pss_change_mib'].append((after['process_sample']['pss_bytes']-o['process_sample']['pss_bytes'])/2**20)
        result['campaigns'][label]={'passed':len(expected),'repeats':repeats}
        for (n,kind,engine,stage),values in sorted(points.items()):
            result['stage_pss_mib'].append({'campaign':label,'rows':n,'kind':kind,'engine':engine,'stage':stage,'values':values,'median':statistics.median(values)})
    assert {label:sum(o['campaign']==label for o in result['excluded_observations']) for label in CAMPAIGNS}=={'memory-localization-pilot-20261031':5,'memory-localization-20261031':22,'memory-ownership-20261031':0,'memory-startup-stable-20261031':0}
    return result

if __name__=='__main__':
    root=Path(__file__).resolve().parent;manifest=json.loads((root/'manifest.json').read_text());payload=(root/'evidence.tar.gz').read_bytes()
    assert hashlib.sha256(payload).hexdigest()==manifest['archive_sha256'];data={}
    with tarfile.open(fileobj=io.BytesIO(payload),mode='r:gz') as archive:
        for member in archive.getmembers():
            path=PurePosixPath(member.name)
            assert member.isfile() and not path.is_absolute() and '..' not in path.parts and member.name not in data
            value=archive.extractfile(member).read();assert hashlib.sha256(value).hexdigest()==manifest['members'][member.name]
            data[member.name]=value
    assert set(data)==set(manifest['members'])
    result=review(data);assert result==json.loads((root/'results.json').read_text())
    print(json.dumps({'replay_passed':True,'trials':sum(x['passed'] for x in result['campaigns'].values()),'production_qualified':False,'performance_qualified':False}))

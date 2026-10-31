from pathlib import Path,PurePosixPath
import hashlib,io,json,re,tarfile

if not __debug__:raise RuntimeError('Verification requires enabled assertions')

def review(data):
    read=lambda name:json.loads(data[name])
    sha=lambda name:hashlib.sha256(data[name]).hexdigest()
    identity=read('identity.json')
    candidate='0be60915b40c9f70f39dcb3a0fd1f8918608a20095f767ec081a5b5320caa277'
    assert identity['candidate_sha256']==candidate and identity['actions_used'] is False
    wrapper=read('reports/guc-registry-reserve-qualification-20261031.json')
    assert wrapper['passed'] and wrapper['candidate_sha256']==candidate
    assert len(wrapper['commands'])==5 and all(c['exit']==0 for c in wrapper['commands'])
    build=read('build/report.json');assert build['passed'] and build['binaries']['postgres']['sha256']==candidate
    for name,expected in build['source_sha256'].items():assert sha('runtime-source/'+name)==expected
    assert {c['label']:c['exit'] for c in build['commands']}=={'guc-suite':0,'allocation-negative-control':101,'name-fold-negative-control':101,'restored-focused':0,'release-build':0}
    assert 'known built-in registry arrays must not reallocate' in data['build/allocation-negative-control.log'].decode()
    assert 'ASCII case folding must not grow a temporary string' in data['build/name-fold-negative-control.log'].decode()
    assert sum(map(int,re.findall(r'test result: ok\. (\d+) passed;',data['build/guc-suite.log'].decode())))==103
    for mode,count in [('full',2),('probes',7)]:
        commands=read('reports/guc-registry-reserve-'+mode+'-commands-20261031.json')
        assert len(commands)==count and all(c['exit']==0 for c in commands)
    full=read('reports/guc-registry-reserve-full-suites-20261031.json')
    assert full['capture_complete'] and full['passed'] is False and len(full['checks'])==12
    assert full['regression']['returncode']==1 and full['regression']['profile']=='postgresql-raw'
    assert full['isolation']['returncode']==0 and full['isolation']['profile']=='engine-overlay'
    raw=data['raw/regression.out'].decode()
    assert len(re.findall(r'^not ok ',raw,re.M))==28 and len(re.findall(r'^ok ',raw,re.M))==203
    iso=data['suite-logs/isolation.log'].decode()
    assert len(re.findall(r'^ok ',iso,re.M))==510 and '# All 510 tests passed.' in iso
    assert len([n for n in data if n.startswith('isolation-results/') and n.endswith('.out')])==510
    audit=read('audit/summary.json')
    assert audit['passed'] and audit['total']==audit['accepted']==231 and audit['unresolved']==[]
    assert audit['raw_candidate_verdict'] is False and len(audit['cases'])==231
    assert audit['reference_report_sha256']==sha('native-report.json')
    assert audit['candidate_report_sha256']==sha('reports/guc-registry-reserve-full-suites-20261031.json')
    assert read('native-report.json')['passed']
    schedule=[c for line in data['regression-fixtures/parallel_schedule'].decode().splitlines() if line.startswith('test:') for c in line.split()[1:]]
    assert [c['case'] for c in audit['cases']]==schedule and len(set(schedule))==231
    exact_tails={'prepared_xacts':16,'psql':1461,'collate.linux.utf8':15,'collate.windows.win1252':16,'numa':10}
    observed_tails={}
    for case in audit['cases']:
        name=case['case'];assert case['passed']
        for field,path in [('source_sha256','regression-fixtures/sql/'+name+'.sql'),('native_sha256','native-results/'+name+'.out'),('candidate_sha256','candidate-results/'+name+'.out')]:
            assert case[field]==sha(path),(name,field)
        proof=read('audit/'+name+'.json')
        assert proof['equal_after_permitted_order'] and proof['refused']==case['refused']
        if case['refused']:
            assert case['raw_equal'] and case['upstream_exact']
            assert data['candidate-results/'+name+'.out']==data['native-results/'+name+'.out']==data['regression-fixtures/expected/'+case['base_expectation']]
            assert case['refused']==[{'sql_line':exact_tails[name],'reason':'SQL echo missing; remaining output stays exact'}]
            observed_tails[name]=exact_tails[name]
        assert not data['audit/'+name+'.diff'].strip()
    assert observed_tails==exact_tails
    counts={}
    for name,count in [('idle-contract',12),('connection-contract',12),('admission',52),('compatibility',87),('recovery',12),('auth-tls',21)]:
        d=read('reports/guc-registry-reserve-'+name+'-20261031.json')
        assert d['passed'] and len(d['checks'])==count and not d.get('failures')
        if 'binary_sha256' in d:assert d['binary_sha256']==candidate
        counts[name]=count
    window=read('reports/guc-registry-reserve-window-peer-20261031.json')
    assert window['passed'] and window['reference_profile']=='patched-postgresql'
    assert window['servers']['candidate']['binary_sha256']==candidate
    assert window['servers']['postgresql']['binary_sha256']==identity['patched_reference_sha256']
    assert all(v['checks']==v['passed']==220 and not v['failures'] for v in window['servers'].values())
    return {'candidate_sha256':candidate,'reviewed_sql_cases':231,'raw_postgresql_passes':203,'raw_postgresql_differences':28,'isolation_cases':510,'smoke_checks':12,'probes':counts,'window_checks_per_engine':220,'production_qualified':False}

if __name__=='__main__':
    root=Path(__file__).resolve().parent;manifest=json.loads((root/'manifest.json').read_text());payload=(root/'evidence.tar.gz').read_bytes()
    assert hashlib.sha256(payload).hexdigest()==manifest['archive_sha256']
    data={}
    with tarfile.open(fileobj=io.BytesIO(payload),mode='r:gz') as archive:
        for item in archive.getmembers():
            p=PurePosixPath(item.name)
            assert item.isfile() and not p.is_absolute() and '..' not in p.parts and item.name not in data
            content=archive.extractfile(item).read();assert hashlib.sha256(content).hexdigest()==manifest['members'][item.name]
            data[item.name]=content
    assert set(data)==set(manifest['members'])
    result=review(data);assert result==json.loads((root/'results.json').read_text())
    print(json.dumps({'replay_passed':True,'members':len(data),**result}))

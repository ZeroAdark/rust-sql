#!/usr/bin/env python3
"""Replay MERGE's real setup, then check trigger traces, actions and atomicity."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess

import psycopg
from psycopg import pq
from cluster import Cluster
from compare_regression import find_anchor,physical_lines,statements
from merge_trace import digest,expected_actions,policy,valid_trace


ROOT=Path(__file__).resolve().parents[2]


def next_state(index,target,source):
    state=target.copy();counts=dict.fromkeys(('inserted','updated','deleted','skipped'),0)
    keys=(target.keys() & source.keys() if index<3 else source.keys() if index<5 else
          target.keys() if index==5 else target.keys() | source.keys() if index==6 else [])
    for key in keys:
        matched=key in target and key in source
        update=(index==0 or index in (1,2,4) and key<10) and matched
        delete=(index==2 and 10<=key<=20 or index==4 and 30<=key<=40) and matched
        delete |= index in (5,6) and key not in source and key<10
        insert=key not in target and key in source and (index==3 and key<10 or index in (4,6) and key<20)
        if update:state[key]+=1;counts['updated']+=1
        elif delete:del state[key];counts['deleted']+=1
        elif insert:state[key]=source[key];counts['inserted']+=1
        else:counts['skipped']+=1
    return state,counts


def run(args,binary,reference):
    rule=policy();fixtures=ROOT/'crates/postgres-18.6-reference/src/test/regress'
    source=(fixtures/'sql/merge.sql').read_text();expected=(fixtures/'expected/merge.out').read_text()
    assert digest(source)==rule['source_sha256'] and digest(expected)==rule['expected_sha256']
    commands={line:sql for line,_,sql in statements(source)};assert commands[665]=='BEGIN;'
    prefix=''.join(source.splitlines(keepends=True)[:664]);plain=physical_lines(expected);raw=physical_lines(expected,keepends=True);pos=0
    for line,anchor,sql in statements(source):
        start=find_anchor(plain,anchor,pos);assert start is not None
        if line==665:expected_prefix=''.join(raw[:start]);break
        pos=start+len(anchor)
    checks=[]
    with Cluster(binary,args.work_dir,reference=reference,settings=['autovacuum=off']) as cluster:
        # Match the upstream test_setup.sql privilege inside this owned cluster.
        with psycopg.connect(cluster.dsn,autocommit=True) as connection:connection.execute('GRANT ALL ON SCHEMA public TO public')
        replay=subprocess.run([str(args.reference_bin/'psql'),'-X','-a','-q'],input=prefix,text=True,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,env=cluster.env,timeout=90)
        (cluster.root/'prefix.out').write_text(replay.stdout);replay.check_returncode()
        assert replay.stdout==expected_prefix,(reference,'prefix mismatch',cluster.root)
        checks.append({'name':'fixture_prefix','through_sql_line':664,'exact':True,'sha256':digest(replay.stdout)})
        notices=[]
        with psycopg.connect(cluster.dsn,autocommit=True) as connection:
            connection.execute('SET SESSION AUTHORIZATION regress_merge_privs')
            connection.add_notice_handler(lambda notice:notices.append(notice.message_primary))
            before_target=connection.execute('SELECT * FROM target ORDER BY tid').fetchall()
            before_source=connection.execute('SELECT * FROM source ORDER BY sid').fetchall()
            assert before_target==[tuple(row) for row in rule['target_rows']]
            row_type=connection.execute("SELECT 'target'::regtype::oid").fetchone()[0]
            connection.execute(commands[665]);connection.execute(commands[666]);notices.clear()
            assert connection.execute('SELECT * FROM source ORDER BY sid').fetchall()==[tuple(row) for row in rule['source_rows']]
            cursor=connection.execute(commands[667],prepare=False);rows=cursor.fetchall();types=[column.type_code for column in cursor.description]
            assert types==[25,row_type,row_type,23,23] and cursor.statusmessage=='MERGE 4'
            assert valid_trace(notices,rows,rule['target_rows'],rule['source_rows']),(notices,rows)
            final=connection.execute(commands[681]).fetchall()
            actions=expected_actions(rule['target_rows'],rule['source_rows'])
            expected_final=sorted((row[3],row[4]) for row in actions.values() if row[0]!='DELETE')
            assert final==expected_final
            trace=notices.copy();connection.execute(commands[682])
            assert connection.execute('SELECT * FROM target ORDER BY tid').fetchall()==before_target
            assert connection.execute('SELECT * FROM source ORDER BY sid').fetchall()==before_source
            checks.append({'name':'trigger_returning_rollback','types':types,'target_type_oid':row_type,'before_target':before_target,'before_source':before_source,'rows':rows,'notices':trace,'final':final,'rollback_restored':True})
            connection.execute('BEGIN');connection.execute('INSERT INTO source VALUES(3,20)');connection.execute('SAVEPOINT duplicate_source')
            try:connection.execute(commands[667],prepare=False)
            except psycopg.Error as error:
                assert error.sqlstate=='21000' and error.diag.message_primary=='MERGE command cannot affect row a second time'
            else:raise AssertionError('duplicate-source error missing')
            assert connection.info.transaction_status==pq.TransactionStatus.INERROR
            connection.execute('ROLLBACK TO SAVEPOINT duplicate_source')
            assert connection.execute('SELECT * FROM target ORDER BY tid').fetchall()==before_target
            connection.execute('ROLLBACK')
            assert connection.execute('SELECT * FROM source ORDER BY sid').fetchall()==before_source
            checks.append({'name':'duplicate_source_atomicity','sqlstate':'21000','target_restored':True,'source_restored':True})
            setup=[sql for line,sql in commands.items() if line<1071 and sql.startswith(('CREATE TABLE ex_m','INSERT INTO ex_m'))]
            assert len(setup)==4
            for sql in setup:connection.execute(sql)
            connection.execute(commands[1071])
            target={i:i*10 for i in range(1,100,2)};source_rows={i:i*10 for i in range(1,101)}
            for index,line in enumerate((1087,1093,1099,1107,1113,1123,1129,1137)):
                call=commands[line];assert call.startswith("SELECT explain_merge('") and call.endswith("');")
                query=call[len("SELECT explain_merge('"):-3].replace("''", "'")
                target,counts=next_state(index,target,source_rows);state=sorted(target.items())
                connection.execute('BEGIN');connection.execute('SAVEPOINT helper_execution')
                cursor=connection.execute(call,prepare=False);text_plan=[row[0] for row in cursor.fetchall()]
                assert [column.type_code for column in cursor.description]==[25]
                expected_counts='Tuples: '+' '.join(f'{key}={value}' for key,value in counts.items() if value)
                assert [row.strip() for row in text_plan if row.strip().startswith('Tuples:')]==([expected_counts] if any(counts.values()) else [])
                assert text_plan[0].endswith('(actual rows=0.00 loops=1)')
                assert connection.execute('SELECT * FROM ex_mtarget ORDER BY a').fetchall()==state
                connection.execute('ROLLBACK TO SAVEPOINT helper_execution')
                document=connection.execute('EXPLAIN (ANALYZE,VERBOSE,FORMAT JSON,TIMING OFF,BUFFERS OFF) '+query).fetchone()[0][0]
                plan=document['Plan'];assert plan['Actual Rows']==0 and plan['Actual Loops']==1
                for key,value in counts.items():assert plan.get('Tuples '+key.title(),0)==value,(line,key,plan)
                cursor=connection.execute('SELECT * FROM ex_mtarget ORDER BY a');actual=cursor.fetchall()
                assert [column.type_code for column in cursor.description]==[23,23] and actual==state
                connection.execute('COMMIT')
                checks.append({'name':f'helper_{line}','sql_line':line,'counts':counts,'state_rows':len(actual),'state_sha256':digest(json.dumps(actual)),'text_plan':text_plan,'explain':document})
        with (Path(binary)/'postgres').open('rb') as executable:binary_hash=hashlib.file_digest(executable,'sha256').hexdigest()
        result={'version':cluster.version,'cluster':str(cluster.root),'binary_sha256':binary_hash,'source_sha256':digest(source),'checks':checks}
    return result


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    for name in ('reference-bin','candidate-bin','work-dir','output'):parser.add_argument('--'+name,type=Path,required=True)
    args=parser.parse_args();report={'passed':False,'engines':{}}
    try:
        for name,binary,reference in (('native',args.reference_bin,True),('candidate',args.candidate_bin,False)):report['engines'][name]=run(args,binary,reference)
        report['passed']=True
    except Exception as error:report['error']=repr(error);raise
    finally:
        args.output.parent.mkdir(parents=True,exist_ok=True);args.output.write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps({'passed':True,'checks':{name:len(engine['checks']) for name,engine in report['engines'].items()}}))


if __name__=='__main__':main()

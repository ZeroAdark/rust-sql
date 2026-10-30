#!/usr/bin/env python3
"""Typed parallel-plan results, worker counters, errors and role recovery."""
import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path

import psycopg
from psycopg import pq
from aggregate_strategy import DATA, SETUP, fixture_sum, policy as input_policy
from cluster import Cluster
from compare_regression import statements
from explain_contract import nodes, UNMEASURED
from worker_context import digest, policy as context_policy


ROOT = Path(__file__).resolve().parents[2]
SORT_SQL = "select * from (select ten from tenk1 where ten < 100 order by ten) ss right join (values (1),(2),(3)) v(x) on true"
ROLE_SQL = "SELECT current_user,current_setting('role'),session_user"
STATS_SQL = "SELECT parallel_workers_to_launch,parallel_workers_launched FROM pg_stat_database WHERE datname=current_database()"


def data_check(connection, evidence, name, sql, expected, types, ordered=True):
    cursor = connection.execute(sql,prepare=False)
    rows = cursor.fetchall();actual_types = [column.type_code for column in cursor.description]
    assert actual_types == types and (rows == expected if ordered else Counter(rows) == Counter(expected)), (name,actual_types,rows[:10])
    canonical = rows if ordered else sorted(rows)
    evidence.append({"name":name,"sql":sql,"types":types,"rows":len(rows),"values_sha256":digest(json.dumps(canonical))})


def error_check(connection,evidence,name,sql,state,message,context,protocol):
    if protocol == "simple":
        result = connection.pgconn.exec_(sql.encode())
        assert result.status == pq.ExecStatus.FATAL_ERROR, (name,result.status)
        fields = {key:result.error_field(field) for key,field in (
            ("state",pq.DiagnosticField.SQLSTATE),("message",pq.DiagnosticField.MESSAGE_PRIMARY),("context",pq.DiagnosticField.CONTEXT))}
        fields = {key:value.decode() if value is not None else None for key,value in fields.items()}
    else:
        try:
            connection.execute(sql,prepare=False)
        except psycopg.Error as error:
            fields = {"state":error.sqlstate,"message":error.diag.message_primary,"context":error.diag.context}
        else:
            raise AssertionError((name,"error missing"))
    assert fields == {"state":state,"message":message,"context":context}, (name,fields)
    evidence.append({"name":name,"protocol":protocol,"sql_sha256":digest(sql),"sqlstate":state,
                     "message_bytes":len(message.encode()),"message_sha256":digest(message),"context":context,
                     "transaction_status":connection.info.transaction_status.name})


def run(args,binary,reference):
    rule = context_policy();inputs = input_policy()
    source = (ROOT/'crates/postgres-18.6-reference/src/test/regress/sql/select_parallel.sql').read_text()
    assert digest(source) == rule['source_sha256'] and digest(SETUP.read_text()) == inputs['setup_lf_sha256']
    assert fixture_sum(DATA,inputs['data_lf_sha256']) == 495000
    by_line = {line:sql for line,_,sql in statements(source)}
    data = DATA.read_text();rows = [line.split('\t') for line in data.splitlines()]
    checks = []
    with Cluster(binary,args.work_dir,reference=reference,settings=['autovacuum=off']) as cluster:
        with psycopg.connect(cluster.dsn,autocommit=True) as connection:
            setup = SETUP.read_text();start = setup.index('CREATE TABLE tenk1 (')
            connection.execute(setup[start:setup.index(');',start)+2])
            with connection.cursor().copy('COPY tenk1 FROM STDIN') as copy:copy.write(data)
            connection.execute('CREATE INDEX ON tenk1(unique1)')
            connection.execute('VACUUM ANALYZE tenk1')
            connection.execute('ALTER TABLE tenk1 SET(parallel_workers=4)')
            connection.execute("SET statement_timeout='60s'; SET parallel_setup_cost=0; SET parallel_tuple_cost=0; SET min_parallel_table_scan_size=0; SET max_parallel_workers_per_gather=4")
            connection.execute('SELECT pg_stat_force_next_flush()')
            before = connection.execute(STATS_SQL).fetchone()
            data_check(connection,checks,'pinned_input','SELECT unique1,hundred,stringu1 FROM tenk1 ORDER BY unique1',
                       sorted((int(row[0]),int(row[6]),row[13]) for row in rows),[23,23,19])
            connection.execute('SET enable_indexscan=off; SET enable_hashjoin=off; SET enable_mergejoin=off; SET enable_material=off')
            expected = [(int(row[4]),x) for row in rows for x in (1,2,3)]
            data_check(connection,checks,'rescanned_sort_values',SORT_SQL,expected,[23,23],ordered=False)
            document = connection.execute('EXPLAIN (ANALYZE,VERBOSE,FORMAT JSON,TIMING OFF,BUFFERS OFF) '+SORT_SQL).fetchone()[0][0]
            root = document['Plan'];visited = list(nodes(root))
            assert root['Actual Rows']==30000 and root['Actual Loops']==1
            assert {'Sort','Nested Loop'} <= {node['Node Type'] for node in visited}
            workers = sum(node.get('Workers Launched',0) for node in visited)
            assert (workers>0 if reference else workers==0),document
            if not reference:
                for node in visited:assert not UNMEASURED.get(node['Node Type'],set()) & node.keys()
            checks.append({'name':'rescanned_sort_counters','explain':document})
            connection.execute('RESET enable_indexscan; RESET enable_hashjoin; RESET enable_mergejoin; RESET enable_material')
            connection.execute('CREATE ROLE regress_parallel_worker')
            for line in (534,538):connection.execute(by_line[line])
            value = next(row[13] for row in rows if int(row[0])==1) + 'abcd'*5000
            long_message = f'invalid input syntax for type smallint: "{value}"'
            for debug in (0,1):
                connection.execute(f'SET debug_parallel_query={debug}')
                data_check(connection,checks,f'function_role_{debug}','SELECT set_and_report_role()',[('regress_parallel_worker',)],[25])
                data_check(connection,checks,f'caller_role_after_success_{debug}',ROLE_SQL,[('rustsql','none','rustsql')],[19,25,19])
                for protocol in ('simple','extended'):
                    suffix = 'parallel worker' if reference and debug else None
                    error_check(connection,checks,f'large_error_{debug}_{protocol}',by_line[460],'22P02',long_message,suffix,protocol)
                    function_context = 'SQL function "set_role_and_error" statement 1' + ('\nparallel worker' if suffix else '')
                    error_check(connection,checks,f'function_error_{debug}_{protocol}',by_line[547],'22012','division by zero',function_context,protocol)
                    data_check(connection,checks,f'caller_role_after_failure_{debug}_{protocol}',ROLE_SQL,[('rustsql','none','rustsql')],[19,25,19])
            connection.execute('BEGIN');connection.execute('SAVEPOINT error_boundary')
            function_context = 'SQL function "set_role_and_error" statement 1' + ('\nparallel worker' if reference else '')
            error_check(connection,checks,'savepoint_failure',by_line[547],'22012','division by zero',function_context,'extended')
            assert connection.info.transaction_status == pq.TransactionStatus.INERROR
            error_check(connection,checks,'aborted_transaction','SELECT 1','25P02','current transaction is aborted, commands ignored until end of transaction block',None,'simple')
            connection.execute('ROLLBACK TO SAVEPOINT error_boundary')
            data_check(connection,checks,'caller_role_after_savepoint',ROLE_SQL,[('rustsql','none','rustsql')],[19,25,19])
            connection.execute('COMMIT');assert connection.info.transaction_status == pq.TransactionStatus.IDLE
            connection.execute('RESET debug_parallel_query')
            connection.execute('SELECT pg_stat_force_next_flush()')
            connection.execute('SELECT pg_stat_clear_snapshot()')
            after = connection.execute(STATS_SQL).fetchone()
            delta = tuple(a-b for a,b in zip(after,before))
            assert (all(value>0 for value in delta) if reference else delta==(0,0)), (before,after)
            cursor = connection.execute('SELECT parallel_workers_to_launch>%s AS wrk_to_launch,parallel_workers_launched>%s AS wrk_launched FROM pg_stat_database WHERE datname=current_database()',before)
            assert cursor.fetchall()==[(reference,reference)] and [c.type_code for c in cursor.description]==[16,16]
            checks.append({'name':'worker_statistics','before':before,'after':after,'delta':delta,'types':[16,16],'values':[reference,reference]})
        with (Path(binary)/'postgres').open('rb') as executable:binary_hash=hashlib.file_digest(executable,'sha256').hexdigest()
        result = {'version':cluster.version,'cluster':str(cluster.root),'binary_sha256':binary_hash,'source_sha256':digest(source),
                  'data_lf_sha256':inputs['data_lf_sha256'],'setup_lf_sha256':inputs['setup_lf_sha256'],'checks':checks}
    return result


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    for name in ('reference-bin','candidate-bin','work-dir','output'):parser.add_argument('--'+name,type=Path,required=True)
    args=parser.parse_args();report={'passed':False,'engines':{}}
    try:
        for name,binary,reference in (('native',args.reference_bin,True),('candidate',args.candidate_bin,False)):report['engines'][name]=run(args,binary,reference)
        report['passed']=True
    except Exception as error:
        report['error']=repr(error);raise
    finally:
        args.output.parent.mkdir(parents=True,exist_ok=True);args.output.write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps({'passed':True,'checks':{name:len(engine['checks']) for name,engine in report['engines'].items()}}))


if __name__ == '__main__':main()

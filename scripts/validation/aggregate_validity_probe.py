#!/usr/bin/env python3
"""Compare aggregate validity, FILTER and NULL results with native PostgreSQL."""
import argparse
import hashlib
import json
from pathlib import Path
import resource

import psycopg
from cluster import Cluster


SETUP = """
CREATE ACCESS METHOD pgrcolumnar TYPE TABLE HANDLER heap_tableam_handler;
CREATE TABLE facts(id integer,k integer,v integer) USING pgrcolumnar;
INSERT INTO facts SELECT i,CASE WHEN i%11=0 THEN NULL ELSE i%31 END,
 CASE WHEN i%31=0 OR i%13=0 THEN NULL ELSE i%19-9 END
 FROM generate_series(1,20000) g(i);
CREATE TABLE dim(k integer PRIMARY KEY);
INSERT INTO dim SELECT i FROM generate_series(0,40) g(i);
VACUUM ANALYZE facts;
VACUUM ANALYZE dim;
"""


def cases():
    folds = 'count(*),count(v),sum(v),min(v),max(v),sum(v::bigint),sum(v::numeric),avg(v::numeric)'
    filters = ','.join(f'sum(v) FILTER(WHERE id%17={i})' for i in range(17))
    for predicate in ('true', 'k=0', 'id%7=0', 'false'):
        yield 'groups/' + predicate, f'SELECT k,{folds} FROM facts WHERE {predicate} GROUP BY k ORDER BY k NULLS FIRST'
        yield 'scalar/' + predicate, f'SELECT {folds} FROM facts WHERE {predicate}'
        yield 'wide/' + predicate, f'SELECT k,{filters} FROM facts WHERE {predicate} GROUP BY k ORDER BY k NULLS FIRST'
        yield 'filtered/' + predicate, f'''SELECT k,count(*) FILTER(WHERE false),count(v) FILTER(WHERE id%3=0),
          sum(v) FILTER(WHERE id%3=0),sum(v) FILTER(WHERE false),
          min(v) FILTER(WHERE v>0),max(v) FILTER(WHERE v<0)
          FROM facts WHERE {predicate} GROUP BY k ORDER BY k NULLS FIRST'''
    yield 'outer_join', f'''SELECT d.k,{folds} FROM dim d LEFT JOIN facts f ON f.k=d.k
      GROUP BY d.k ORDER BY d.k'''
    yield 'grouping_sets', f'''SELECT k,grouping(k),{folds} FROM facts GROUP BY GROUPING SETS((k),())
      ORDER BY grouping(k),k NULLS FIRST'''
    yield 'distinct', '''SELECT k,count(DISTINCT v),sum(DISTINCT v),
      array_agg(DISTINCT v ORDER BY v) FROM facts GROUP BY k ORDER BY k NULLS FIRST'''
    yield 'nested', '''SELECT x,count(*),sum(n),sum(s),min(s),max(s) FROM
      (SELECT k%7 AS x,k,count(v) AS n,sum(v) AS s FROM facts GROUP BY k) g
      GROUP BY x ORDER BY x NULLS FIRST'''
    yield 'correlated', '''SELECT d.k,(SELECT sum(v) FROM facts f WHERE f.k=d.k),
      (SELECT count(v) FROM facts f WHERE f.k=d.k) FROM dim d ORDER BY d.k'''
    yield 'high_cardinality', '''SELECT id/2,count(*),count(v),sum(v) FROM facts
      GROUP BY id/2 ORDER BY id/2'''


def answer(connection, sql, prepared):
    cursor = connection.execute(sql, prepare=prepared)
    return {'types': [column.type_code for column in cursor.description],
            'rows': [[repr(value) for value in row] for row in cursor.fetchall()]}


def run(args):
    report = {'passed': False, 'checks': [], 'failures': [], 'binaries': {}}
    for name in ('reference', 'candidate'):
        binary = (getattr(args, name) / 'postgres').resolve()
        with binary.open('rb') as stream:
            report['binaries'][name] = {'path': str(binary), 'sha256': hashlib.file_digest(stream, 'sha256').hexdigest()}
    with Cluster(args.reference, args.work_dir, reference=True, settings=['autovacuum=off']) as pg, \
            Cluster(args.candidate, args.work_dir, settings=['autovacuum=off']) as candidate:
        report['clusters'] = {'reference': str(pg.root), 'candidate': str(candidate.root)}
        report['versions'] = {'reference': pg.version, 'candidate': candidate.version}
        with psycopg.connect(pg.dsn, autocommit=True) as pc, psycopg.connect(candidate.dsn, autocommit=True) as cc:
            for connection in (pc, cc):
                connection.execute("SET statement_timeout='60s'; SET max_parallel_workers_per_gather=0")
                # VACUUM must run outside a multi-statement implicit transaction.
                for statement in SETUP.split(';'):
                    if statement.strip():
                        connection.execute(statement, prepare=False)
            # Small work_mem also exercises grouping with many more groups than the measured CPU workload.
            for memory in ('64kB', '16MB'):
                for connection in (pc, cc):
                    connection.execute(f"SET work_mem='{memory}'")
                for prepared in (False, True):
                    for name, sql in cases():
                        expected, actual = answer(pc, sql, prepared), answer(cc, sql, prepared)
                        check = {'name': name, 'work_mem': memory, 'prepared': prepared, 'sql': sql}
                        if expected != actual:
                            report['failures'].append({**check, 'reference': expected, 'candidate': actual})
                        else:
                            check.update(types=actual['types'], rows=len(actual['rows']),
                                         result_sha256=hashlib.sha256(json.dumps(actual, sort_keys=True).encode()).hexdigest())
                            report['checks'].append(check)
    report['passed'] = len(report['checks']) == 4 * len(list(cases())) and not report['failures']
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('reference', 'candidate', 'work-dir', 'output'):
        parser.add_argument('--' + name, type=Path, required=True)
    args = parser.parse_args()
    resource.setrlimit(resource.RLIMIT_STACK, (65520 * 1024, resource.getrlimit(resource.RLIMIT_STACK)[1]))
    report = run(args)
    args.output.write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps({'passed': report['passed'], 'checks': len(report['checks']), 'failures': len(report['failures'])}))
    return 0 if report['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())

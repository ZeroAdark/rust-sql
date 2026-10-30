#!/usr/bin/env python3
"""Exercise all pinned window traces, typed inputs and actual evaluation orders."""
import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path
import random
import re

import psycopg
from psycopg.types.string import TextLoader
from cluster import Cluster
from compare_regression import statements
from window_trace import FIXTURES, digest, inputs, policy, prove, values


def cells(rows):
    return [['' if value is None else str(value) for value in row] for row in rows]


def run(args, binary, reference):
    spec, data = policy(), inputs()
    commands = {line: sql for line, _, sql in statements((FIXTURES / 'sql/window.sql').read_text())}
    setup = (FIXTURES / 'sql/test_setup.sql').read_text()
    tenk_text = (FIXTURES / 'data/tenk.data').read_text()
    tenk_rows = [row.split('\t') for row in tenk_text.splitlines()]
    employees = values(commands[spec['employee_insert_line']])
    datetimes = values(commands[spec['datetime_insert_line']])
    checks = []
    with Cluster(binary, args.work_dir, reference=reference, settings=['autovacuum=off']) as cluster:
        with psycopg.connect(cluster.dsn, autocommit=True) as connection:
            # Retain type OIDs while reading the fixture's exact textual forms,
            # including datetime infinities unsupported by Python datetime.
            for oid in (1082, 1083, 1266, 1186, 1184, 1114):
                connection.adapters.register_loader(oid, TextLoader)
            connection.execute("SET DateStyle='Postgres,MDY'; SET IntervalStyle='postgres_verbose'; SET TimeZone='PST8PDT'; SET statement_timeout='60s'; SET work_mem='64kB'")
            start = setup.index('CREATE TABLE tenk1 (')
            connection.execute(setup[start:setup.index(');', start) + 2])
            with connection.cursor().copy('COPY tenk1 FROM STDIN') as copy:
                copy.write(tenk_text)
            connection.execute('CREATE INDEX ON tenk1(unique1); CREATE INDEX ON tenk1(unique2)')
            connection.execute('VACUUM ANALYZE tenk1')
            cursor = connection.execute('SELECT unique1,unique2,four,ten FROM tenk1 ORDER BY unique1')
            actual = cursor.fetchall()
            assert [column.type_code for column in cursor.description] == [23] * 4
            assert actual == sorted(tuple(int(row[i]) for i in (0, 1, 3, 4)) for row in tenk_rows)
            checks.append({'name': 'tenk_input', 'rows': len(actual), 'values_sha256': digest(json.dumps(actual))})
            for sql in commands.values():
                if sql.startswith(('CREATE TEMPORARY TABLE empsalary (', 'create temp table datetimes(')):
                    connection.execute(sql)
            for round_number in range(args.rounds):
                connection.execute('TRUNCATE empsalary,datetimes')
                if round_number == 0:
                    connection.execute(commands[spec['employee_insert_line']])
                    connection.execute(commands[spec['datetime_insert_line']])
                else:
                    employee_order, datetime_order = employees.copy(), datetimes.copy()
                    random.Random(args.seed + round_number).shuffle(employee_order)
                    random.Random(args.seed + round_number).shuffle(datetime_order)
                    with connection.cursor() as cursor:
                        cursor.executemany('INSERT INTO empsalary VALUES(%s,%s,%s,%s)', employee_order)
                        cursor.executemany('INSERT INTO datetimes VALUES(%s,%s,%s,%s,%s,%s)', datetime_order)
                cursor = connection.execute('SELECT * FROM empsalary')
                assert [column.type_code for column in cursor.description] == [1043, 20, 23, 1082]
                employee_cells = cells(cursor.fetchall())
                assert Counter(map(tuple, employee_cells)) == Counter(data['employee_rows'])
                checks.append({'name': 'employee_input', 'round': round_number, 'rows': employee_cells})
                cursor = connection.execute('SELECT * FROM datetimes ORDER BY id')
                assert [column.type_code for column in cursor.description] == [23, 1083, 1266, 1186, 1184, 1114]
                datetime_cells = cells(cursor.fetchall())
                expected = [[str(i), *[data[kind][i][0][1] for kind in ('time', 'timetz', 'interval', 'timestamptz', 'timestamp')]] for i in range(12)]
                assert datetime_cells == expected
                checks.append({'name': 'datetime_input', 'round': round_number, 'rows': datetime_cells})
                for rule in spec['statements']:
                    for companion in (False, True) if rule['kind'] == 'frame' else (False,):
                        sql = rule['sql']
                        if companion:
                            sql = re.sub(r'^select\b', f"SELECT row_number() OVER(ORDER BY {rule['sort_sql']}) AS evaluation_order,", sql, count=1, flags=re.I)
                        cursor = connection.execute(sql, prepare=False)
                        types = [column.type_code for column in cursor.description]
                        assert types == ([20] if companion else []) + rule['type_oids'], (rule['sql_line'], types)
                        rows = cursor.fetchall()
                        if companion:
                            assert [row[0] for row in rows] == list(range(1, len(rows) + 1))
                            rows = [row[1:] for row in rows]
                        rendered = cells(rows)
                        witness = prove(rule, rendered, data)
                        assert witness is not None, (rule['sql_line'], round_number, companion, rendered)
                        checks.append({'name': 'window_trace', 'sql_line': rule['sql_line'], 'round': round_number,
                                       'evaluation_order_column': companion, 'types': types, 'rows': rendered, 'witness': witness})
        with (Path(binary) / 'postgres').open('rb') as executable:
            binary_hash = hashlib.file_digest(executable, 'sha256').hexdigest()
        result = {'version': cluster.version, 'cluster': str(cluster.root), 'binary_sha256': binary_hash,
                  'source_sha256': spec['source_sha256'], 'data_lf_sha256': spec['data_lf_sha256'],
                  'setup_lf_sha256': spec['setup_lf_sha256'], 'checks': checks}
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('reference-bin', 'candidate-bin', 'work-dir', 'output'):
        parser.add_argument('--' + name, type=Path, required=True)
    parser.add_argument('--rounds', type=int, default=3)
    parser.add_argument('--seed', type=int, default=30102026)
    args = parser.parse_args()
    if args.rounds < 1:
        parser.error('--rounds must be positive')
    report = {'passed': False, 'rounds': args.rounds, 'seed': args.seed, 'engines': {}}
    try:
        for name, binary, reference in (('native', args.reference_bin, True), ('candidate', args.candidate_bin, False)):
            report['engines'][name] = run(args, binary, reference)
        report['passed'] = True
    except Exception as error:
        report['error'] = repr(error)
        raise
    finally:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps({'passed': True, 'checks': {name: len(engine['checks']) for name, engine in report['engines'].items()}}))


if __name__ == '__main__':
    main()

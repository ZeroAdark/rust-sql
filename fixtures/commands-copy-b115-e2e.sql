-- bugs/batch-115-backend-commands-copy: C-vs-pgrust parity for the COPY
-- fixes in commands/copy. Expected file captured from C 18.6
-- (scripts/regress-diff.sh --capture).
\set VERBOSITY verbose

-- copyfrom: ExecCloseResultRelations/ExecCleanupTupleRouting release the
-- RowExclusiveLock ExecOpenIndices took on the target's (and routed leaves')
-- indexes; only the heap locks live to end of xact
CREATE TABLE b115_t(a int);
CREATE INDEX b115_ti ON b115_t(a);
CREATE TABLE b115_p(a int) PARTITION BY RANGE (a);
CREATE TABLE b115_p1 PARTITION OF b115_p FOR VALUES FROM (0) TO (10);
CREATE INDEX b115_pi ON b115_p(a);
BEGIN;
COPY b115_t FROM stdin;
1
\.
SELECT relation::regclass, mode FROM pg_locks WHERE pid = pg_backend_pid() AND locktype = 'relation' AND relation::regclass::text LIKE 'b115%' ORDER BY 1, 2;
COPY b115_p FROM stdin;
1
\.
SELECT relation::regclass, mode FROM pg_locks WHERE pid = pg_backend_pid() AND locktype = 'relation' AND relation::regclass::text LIKE 'b115%' ORDER BY 1, 2;
ROLLBACK;
DROP TABLE b115_t, b115_p;

-- copyto: every finished row is fwritten; a query error leaves the rows
-- before it in the file (fclose at abort flushes the stdio buffer)
COPY (SELECT CASE WHEN i = 20 THEN (1 / (i - i))::text ELSE repeat('x', 1024) END FROM generate_series(1, 20) AS g(i)) TO '/tmp/commands-copy-b115-partial.txt';
SELECT length(pg_read_binary_file('/tmp/commands-copy-b115-partial.txt'));
COPY (SELECT repeat('y', 100) FROM generate_series(1, 3)) TO '/tmp/commands-copy-b115-partial.txt';
SELECT length(pg_read_binary_file('/tmp/commands-copy-b115-partial.txt'));

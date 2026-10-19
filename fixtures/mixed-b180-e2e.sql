-- bugs/batch-180-mixed-xhigh: expected captured from C 18.6
-- (scripts/mixed-b180-e2e.sh).
\set VERBOSITY verbose
-- regression: ExecScanFetch checks for interrupts before every
-- tuple, so a cancel requested while projecting row 1 fires before row 2's
-- division by zero (57014, not 22012), on the qualified batch path too.
CREATE TABLE b180_t5(i int);
INSERT INTO b180_t5 VALUES (1),(2);
SELECT CASE WHEN i = 1 THEN pg_cancel_backend(pg_backend_pid())::int ELSE 1/(i-2) END FROM b180_t5 WHERE i > 0;
SELECT CASE WHEN i = 1 THEN pg_cancel_backend(pg_backend_pid())::int ELSE 1/(i-2) END FROM b180_t5;
SELECT CASE WHEN i = 1 THEN pg_cancel_backend(pg_backend_pid())::int ELSE 1/(i-2) END FROM b180_t5 WHERE i < 3;
DROP TABLE b180_t5;
-- regression / regression: the large-object
-- relation locks belong to the top transaction (open_lo_relation switches to
-- TopTransactionResourceOwner), so ROLLBACK TO SAVEPOINT keeps them.
BEGIN;
SELECT lo_create(180001);
SAVEPOINT a;
SELECT length(lo_get(180001));
SELECT relation::regclass AS rel, mode FROM pg_locks WHERE relation IN ('pg_largeobject'::regclass, 'pg_largeobject_loid_pn_index'::regclass) AND pid = pg_backend_pid() ORDER BY 1::text;
ROLLBACK TO a;
SELECT relation::regclass AS rel, mode FROM pg_locks WHERE relation IN ('pg_largeobject'::regclass, 'pg_largeobject_loid_pn_index'::regclass) AND pid = pg_backend_pid() ORDER BY 1::text;
SAVEPOINT b;
SELECT lo_lseek64(lo_open(180001, 262144), 0, 2);
ROLLBACK TO b;
SELECT relation::regclass AS rel, mode FROM pg_locks WHERE relation IN ('pg_largeobject'::regclass, 'pg_largeobject_loid_pn_index'::regclass) AND pid = pg_backend_pid() ORDER BY 1::text;
SELECT lo_unlink(180001);
COMMIT;
BEGIN;
SAVEPOINT c;
SELECT lo_create(180002);
ROLLBACK TO c;
SELECT relation::regclass AS rel, mode FROM pg_locks WHERE relation IN ('pg_largeobject'::regclass, 'pg_largeobject_loid_pn_index'::regclass) AND pid = pg_backend_pid() ORDER BY 1::text;
COMMIT;
-- regression: an index AM handler declared with SET options runs through
-- fmgr_security_definer, so its proconfig is applied on every handler call
-- (kept last: temp_buffers stays rejected for the rest of the session).
CREATE FUNCTION b180_handler(internal) RETURNS index_am_handler AS 'bthandler' LANGUAGE internal SET temp_buffers = '2000';
CREATE ACCESS METHOD b180_am TYPE INDEX HANDLER b180_handler;
CREATE OPERATOR CLASS b180_int4_ops DEFAULT FOR TYPE int4 USING b180_am AS OPERATOR 1 <, OPERATOR 2 <=, OPERATOR 3 =, OPERATOR 4 >=, OPERATOR 5 >, FUNCTION 1 btint4cmp(int4,int4);
CREATE TABLE b180_t13(a int);
CREATE INDEX b180_t13_i ON b180_t13 USING b180_am(a);
SHOW temp_buffers;
CREATE TEMP TABLE b180_tt13(a int);
INSERT INTO b180_tt13 VALUES (1);
CREATE TABLE b180_t13b(a int);
CREATE INDEX b180_t13b_i ON b180_t13b USING b180_am(a);
SHOW temp_buffers;

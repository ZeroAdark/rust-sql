-- MERGE into a partitioned target with no INSERT action, whose MATCHED row is
-- concurrently deleted or updated: the recheck falls through to NOT MATCHED.
\set VERBOSITY verbose
\setenv PGHOST :HOST
\setenv PGPORT :PORT
\setenv PGUSER :USER
\setenv PGDATABASE :DBNAME
CREATE TABLE wio431_tgt (k int, v int, t text) PARTITION BY RANGE (k);
CREATE TABLE wio431_tgt1 PARTITION OF wio431_tgt FOR VALUES FROM (0) TO (10);
CREATE TABLE wio431_tgt2 PARTITION OF wio431_tgt FOR VALUES FROM (10) TO (100);
CREATE TABLE wio431_src (k int, v int);
INSERT INTO wio431_src VALUES (1, 100), (2, 200), (4, 400);
-- 1. WHEN MATCHED THEN DELETE, concurrent DELETE
DELETE FROM wio431_tgt; INSERT INTO wio431_tgt VALUES (1, 10, 'a'), (2, 20, 'b'), (3, 30, 'c');
BEGIN;
DELETE FROM wio431_tgt WHERE k = 1;
\! F="${REGRESS_DIFF_DIR:-/tmp}/wio431-bob.out"; rm -f "$F"; ("${PGRUST_E2E_PSQL:-psql}" -X -v VERBOSITY=verbose -c "MERGE INTO wio431_tgt t USING wio431_src s ON t.k = s.k WHEN MATCHED THEN DELETE" >"$F" 2>&1 &)
SELECT pg_sleep(1);
COMMIT;
\! F="${REGRESS_DIFF_DIR:-/tmp}/wio431-bob.out"; for i in $(seq 1 300); do [ -s "$F" ] && break; sleep 0.1; done; sleep 0.2; cat "$F"
SELECT * FROM wio431_tgt ORDER BY k;
-- 2. WHEN MATCHED THEN UPDATE, concurrent DELETE
DELETE FROM wio431_tgt; INSERT INTO wio431_tgt VALUES (1, 10, 'a'), (2, 20, 'b'), (3, 30, 'c');
BEGIN;
DELETE FROM wio431_tgt WHERE k = 1;
\! F="${REGRESS_DIFF_DIR:-/tmp}/wio431-bob.out"; rm -f "$F"; ("${PGRUST_E2E_PSQL:-psql}" -X -v VERBOSITY=verbose -c "MERGE INTO wio431_tgt t USING wio431_src s ON t.k = s.k WHEN MATCHED THEN UPDATE SET v = s.v RETURNING merge_action(), t.*" >"$F" 2>&1 &)
SELECT pg_sleep(1);
COMMIT;
\! F="${REGRESS_DIFF_DIR:-/tmp}/wio431-bob.out"; for i in $(seq 1 300); do [ -s "$F" ] && break; sleep 0.1; done; sleep 0.2; cat "$F"
SELECT * FROM wio431_tgt ORDER BY k;
-- 3. MATCHED DELETE plus NOT MATCHED DO NOTHING, concurrent key-moving UPDATE
DELETE FROM wio431_tgt; INSERT INTO wio431_tgt VALUES (1, 10, 'a'), (2, 20, 'b'), (3, 30, 'c');
BEGIN;
UPDATE wio431_tgt SET k = 11 WHERE k = 1;
\! F="${REGRESS_DIFF_DIR:-/tmp}/wio431-bob.out"; rm -f "$F"; ("${PGRUST_E2E_PSQL:-psql}" -X -v VERBOSITY=verbose -c "MERGE INTO wio431_tgt t USING wio431_src s ON t.k = s.k WHEN MATCHED THEN DELETE WHEN NOT MATCHED THEN DO NOTHING" >"$F" 2>&1 &)
SELECT pg_sleep(1);
COMMIT;
\! F="${REGRESS_DIFF_DIR:-/tmp}/wio431-bob.out"; for i in $(seq 1 300); do [ -s "$F" ] && break; sleep 0.1; done; sleep 0.2; cat "$F"
SELECT * FROM wio431_tgt ORDER BY k;
-- 4. control: an INSERT action present, concurrent DELETE
DELETE FROM wio431_tgt; INSERT INTO wio431_tgt VALUES (1, 10, 'a'), (2, 20, 'b'), (3, 30, 'c');
BEGIN;
DELETE FROM wio431_tgt WHERE k = 1;
\! F="${REGRESS_DIFF_DIR:-/tmp}/wio431-bob.out"; rm -f "$F"; ("${PGRUST_E2E_PSQL:-psql}" -X -v VERBOSITY=verbose -c "MERGE INTO wio431_tgt t USING wio431_src s ON t.k = s.k WHEN MATCHED THEN DELETE WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.v, 'n')" >"$F" 2>&1 &)
SELECT pg_sleep(1);
COMMIT;
\! F="${REGRESS_DIFF_DIR:-/tmp}/wio431-bob.out"; for i in $(seq 1 300); do [ -s "$F" ] && break; sleep 0.1; done; sleep 0.2; cat "$F"
SELECT * FROM wio431_tgt ORDER BY k;

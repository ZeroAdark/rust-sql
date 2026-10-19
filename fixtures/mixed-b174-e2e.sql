-- bugs/batch-174-mixed-xhigh: expected captured from C 18.6
-- (scripts/mixed-b174-e2e.sh).
\set VERBOSITY verbose
-- regression: a slot left acquired by a caught 55000 must not
-- trip a release-disabled Assert on the next create/acquire.
SELECT pg_create_physical_replication_slot('b174_a');
DO $$ BEGIN PERFORM pg_replication_slot_advance('b174_a', pg_current_wal_lsn()); EXCEPTION WHEN OTHERS THEN RAISE NOTICE 'caught % %', SQLSTATE, SQLERRM; END $$;
SELECT pg_create_physical_replication_slot('b174_b');
SELECT slot_name, active, active_pid = pg_backend_pid() AS mine FROM pg_replication_slots WHERE slot_name LIKE 'b174%' ORDER BY 1;
SELECT pg_drop_replication_slot('b174_a');
SELECT pg_drop_replication_slot('b174_b');
SELECT pg_create_physical_replication_slot('b174_c');
DO $$ BEGIN PERFORM pg_replication_slot_advance('b174_c', pg_current_wal_lsn()); EXCEPTION WHEN OTHERS THEN RAISE NOTICE 'caught % %', SQLSTATE, SQLERRM; END $$;
SELECT pg_drop_replication_slot('b174_c');
SELECT count(*) FROM pg_replication_slots WHERE slot_name LIKE 'b174%';
-- regression: IN list with a non-constant member shares
-- the left operand across comparisons; the SEARCH/CYCLE rewrite bumps
-- varlevelsup in place.
SELECT * FROM (VALUES (1)) x(a) WHERE EXISTS (WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM t WHERE n < 3 AND x.a IN (n, 2)) SEARCH DEPTH FIRST BY n SET s SELECT * FROM t OFFSET 0);
SELECT * FROM (VALUES (1)) x(a) WHERE EXISTS (WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM t WHERE n < 3 AND x.a IN (n, 2)) CYCLE n SET c USING p SELECT * FROM t OFFSET 0);
SELECT * FROM (VALUES (2)) x(a) WHERE EXISTS (WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM t WHERE n < 3 AND x.a NOT IN (n, 5)) SEARCH BREADTH FIRST BY n SET s SELECT * FROM t OFFSET 0);
-- regression: record / composite fields hold flat arrays (multi-dim
-- insertion into an empty array is allowed); array Vars are expanded (not).
DO $$ DECLARE r record; BEGIN SELECT '{}'::int[] AS a INTO r; r.a[1][2] := 5; RAISE NOTICE 'rec %', r.a; END $$;
DO $$ DECLARE r record; BEGIN SELECT NULL::int[] AS a INTO r; r.a[1][2] := 5; RAISE NOTICE 'recnull %', r.a; END $$;
CREATE TYPE b174_ct AS (a int[]);
DO $$ DECLARE r b174_ct; BEGIN r.a := '{}'; r.a[1][2] := 5; RAISE NOTICE 'ctrow %', r.a; END $$;
DO $$ DECLARE a int[] := '{}'; BEGIN a[1][2] := 5; RAISE NOTICE 'var %', a; END $$;
DO $$ DECLARE a int[]; BEGIN a[1][2] := 5; RAISE NOTICE 'varnull %', a; END $$;
DO $$ DECLARE a int[] := '{}'; BEGIN a[3] := 5; RAISE NOTICE 'var1d %', a; END $$;
-- regression: DROP DATABASE's DropSetting is a heap scan of
-- pg_db_role_setting, never an index scan.
CREATE DATABASE b174_db;
SELECT pg_stat_force_next_flush();
SELECT seq_scan, idx_scan FROM pg_stat_sys_tables WHERE relname = 'pg_db_role_setting' \gset b174_
DROP DATABASE b174_db;
SELECT pg_stat_force_next_flush();
SELECT pg_sleep(0.5);
-- idx_scan is not pinned: every backend start (autovacuum workers included)
-- adds 4 index lookups on pg_db_role_setting.
SELECT seq_scan - :b174_seq_scan AS dseq FROM pg_stat_sys_tables WHERE relname = 'pg_db_role_setting';
-- regression: SerializeTupleReceive is an AllocSet.
CREATE FUNCTION b174_mc() RETURNS int LANGUAGE plpgsql AS $$ DECLARE t text; BEGIN SELECT string_agg(type || '/' || name, ',') INTO t FROM pg_backend_memory_contexts WHERE name = 'SerializeTupleReceive'; RAISE NOTICE 'ctx %', t; RETURN 1; END $$;
EXPLAIN (ANALYZE, SERIALIZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT b174_mc();
DROP FUNCTION b174_mc();
DROP TYPE b174_ct;

-- bugs/batch-219-mixed-reaudit: C-vs-pgrust parity for the executor / dfmgr
-- fixes. Expected file captured from C 18.6 (scripts/regress-diff.sh --capture).
\set VERBOSITY verbose

-- regression: once the window buffer spills, spool_tuples
-- drains the rest of the partition in one go (nodeWindowAgg.c:1331), so the
-- outer plan's error / NOTICEs surface on the FETCH that crosses the spill.
CREATE FUNCTION b219_wf(i int) RETURNS bool LANGUAGE plpgsql AS $$ BEGIN IF i = 3000 THEN RAISE EXCEPTION 'boom'; END IF; RETURN true; END $$;
CREATE FUNCTION b219_wg(i int) RETURNS bool LANGUAGE plpgsql AS $$ BEGIN IF i % 500 = 0 THEN RAISE NOTICE 'g(%)', i; END IF; RETURN true; END $$;
SET work_mem = '64kB';
BEGIN;
DECLARE b219_c CURSOR FOR SELECT sum(i) OVER (ROWS BETWEEN 2500 PRECEDING AND CURRENT ROW) FROM generate_series(1, 5000) i WHERE b219_wf(i);
FETCH 2000 FROM b219_c;
ROLLBACK;
BEGIN;
DECLARE b219_c CURSOR FOR SELECT sum(i) OVER (ROWS BETWEEN 2500 PRECEDING AND CURRENT ROW) FROM generate_series(1, 5000) i WHERE b219_wg(i);
MOVE 1000 FROM b219_c;
MOVE 1000 FROM b219_c;
MOVE 1000 FROM b219_c;
ROLLBACK;
RESET work_mem;

-- regression: MJEvalOuterValues/MJEvalInnerValues evaluate
-- every merge key even when the first one is NULL and ends the join.
CREATE FUNCTION b219_mjf(int) RETURNS int LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RAISE NOTICE 'f(%)', $1; RETURN $1; END $$;
CREATE TABLE b219_mja(x int, y int);
CREATE TABLE b219_mjb(x int, y int);
INSERT INTO b219_mja VALUES (1,1),(2,2),(NULL,3);
INSERT INTO b219_mjb VALUES (1,1),(2,2),(100,100);
CREATE INDEX b219_mja_ix ON b219_mja(x, b219_mjf(y));
CREATE INDEX b219_mjb_ix ON b219_mjb(x, y);
ANALYZE b219_mja;
ANALYZE b219_mjb;
SET enable_hashjoin = off; SET enable_nestloop = off; SET enable_seqscan = off; SET enable_sort = off; SET enable_material = off;
EXPLAIN (COSTS OFF) SELECT a.x, b.y FROM b219_mja a JOIN b219_mjb b ON a.x = b.x AND b219_mjf(a.y) = b.y;
SELECT a.x, b.y FROM b219_mja a JOIN b219_mjb b ON a.x = b.x AND b219_mjf(a.y) = b.y;
SELECT a.x, b.y FROM b219_mjb b JOIN b219_mja a ON a.x = b.x AND b219_mjf(a.y) = b.y;
RESET enable_hashjoin; RESET enable_nestloop; RESET enable_seqscan; RESET enable_sort; RESET enable_material;

-- regression: execTuplesUnequal's equality call goes through
-- FunctionCall2Coll, so a NULL result is XX000 "function N returned NULL".
CREATE FUNCTION b219_nulleq(int, int) RETURNS bool LANGUAGE sql STRICT IMMUTABLE AS 'SELECT NULL::bool';
CREATE OPERATOR === (LEFTARG = int, RIGHTARG = int, FUNCTION = b219_nulleq, HASHES, MERGES);
CREATE OPERATOR CLASS b219_nulleq_btree FOR TYPE int USING btree AS OPERATOR 1 <, OPERATOR 2 <=, OPERATOR 3 ===, OPERATOR 4 >=, OPERATOR 5 >, FUNCTION 1 btint4cmp(int, int);
CREATE OPERATOR CLASS b219_nulleq_hash FOR TYPE int USING hash AS OPERATOR 1 ===, FUNCTION 1 hashint4(int);
DO $$
BEGIN
  PERFORM ROW(1, 2) === ANY (SELECT 1, NULL::int);
EXCEPTION WHEN OTHERS THEN
  RAISE NOTICE '% %', SQLSTATE, regexp_replace(SQLERRM, 'function \d+', 'function N');
END $$;
SELECT 1 AS session_alive;

-- regression: AFTER ROW triggers on a partition fire through a
-- fresh trig-target ResultRelInfo (rootRelInfo = NULL never matches the routed
-- / child entry), reported after the routing section by EXPLAIN ANALYZE.
CREATE TABLE b219_p (a int) PARTITION BY LIST (a);
CREATE TABLE b219_p1 PARTITION OF b219_p FOR VALUES IN (1);
CREATE FUNCTION b219_tf() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
CREATE TRIGGER a_after AFTER INSERT ON b219_p1 FOR EACH ROW EXECUTE FUNCTION b219_tf();
CREATE TRIGGER b_before BEFORE INSERT ON b219_p1 FOR EACH ROW EXECUTE FUNCTION b219_tf();
CREATE TRIGGER c_after AFTER UPDATE ON b219_p1 FOR EACH ROW EXECUTE FUNCTION b219_tf();
CREATE TRIGGER d_before BEFORE UPDATE ON b219_p1 FOR EACH ROW EXECUTE FUNCTION b219_tf();
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) INSERT INTO b219_p VALUES (1);
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) UPDATE b219_p SET a = 1;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) INSERT INTO b219_p1 VALUES (1);
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) UPDATE b219_p1 SET a = 1;

-- regression: the foreign-partition transition-capture
-- refusal (trigger.c:2555/3162) comes after the remote DML, so a remote
-- error is what the user sees.
CREATE EXTENSION postgres_fdw;
DO $$
BEGIN
  EXECUTE format('CREATE SERVER b219_srv FOREIGN DATA WRAPPER postgres_fdw OPTIONS (host %L, port %L, dbname %L)',
                 current_setting('unix_socket_directories'), current_setting('port'), current_database());
  EXECUTE format('CREATE USER MAPPING FOR CURRENT_USER SERVER b219_srv OPTIONS (user %L)', current_user);
END $$;
CREATE TABLE b219_r_p2(id int, v int CHECK (v >= 0));
CREATE TABLE b219_fp(id int, v int) PARTITION BY RANGE(id);
CREATE TABLE b219_fp1 PARTITION OF b219_fp FOR VALUES FROM (0) TO (10);
CREATE FOREIGN TABLE b219_fp2 PARTITION OF b219_fp FOR VALUES FROM (10) TO (20) SERVER b219_srv OPTIONS (table_name 'b219_r_p2');
INSERT INTO b219_fp VALUES (15, 1);
CREATE FUNCTION b219_ff() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$;
CREATE TRIGGER b219_tu AFTER UPDATE ON b219_fp REFERENCING OLD TABLE AS ot FOR EACH STATEMENT EXECUTE FUNCTION b219_ff();
UPDATE b219_fp SET v = -1 * (random()*0+1)::int WHERE id = 15;
UPDATE b219_fp SET v = 1 WHERE id = 15;
CREATE TRIGGER b219_ti AFTER INSERT ON b219_fp REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION b219_ff();
INSERT INTO b219_fp VALUES (16, -5);
INSERT INTO b219_fp VALUES (16, 5);
CREATE TRIGGER b219_td AFTER DELETE ON b219_fp REFERENCING OLD TABLE AS ot FOR EACH STATEMENT EXECUTE FUNCTION b219_ff();
DELETE FROM b219_fp WHERE id = 15;
SELECT * FROM b219_r_p2 ORDER BY id;
DROP FOREIGN TABLE b219_fp2;
DROP TABLE b219_fp, b219_r_p2;
DROP SERVER b219_srv CASCADE;
DROP EXTENSION postgres_fdw;

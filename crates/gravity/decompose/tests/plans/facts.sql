-- The D5 facts corpus (decompose.md §4.8): every index form the facts layer classifies and the shapes whose
-- order provenance reads them — DESC / NULLS FIRST columns (§7 k), expression columns (§7 l), the ordered
-- Append over range partitions (§7 m) — plus INCLUDE, partial, non-btree, exclusion, deferrable and generated
-- columns for the write family's classification. Captured by scripts/gravity-capture-plans.sh facts; the same
-- file is scripts/gravity-facts-check.sh's corpus (the server's facts vs the capture's).
CREATE TABLE fd (id int PRIMARY KEY, a int, b int, t text, p point, g int GENERATED ALWAYS AS (a * 2) STORED);
CREATE INDEX fd_a_desc ON fd (a DESC);
CREATE INDEX fd_b_nf ON fd (b NULLS FIRST);
CREATE INDEX fd_ab ON fd (a DESC NULLS LAST, b ASC NULLS FIRST);
CREATE INDEX fd_expr ON fd ((a + b));
CREATE INDEX fd_lower ON fd (lower(t));
CREATE INDEX fd_mixed ON fd (a, (b * 2) DESC, t);
CREATE INDEX fd_inc ON fd (a) INCLUDE (b, t);
CREATE INDEX fd_partial ON fd (b) WHERE b > 10;
CREATE INDEX fd_hash ON fd USING hash (a);
CREATE INDEX fd_gin ON fd USING gin (to_tsvector('simple'::regconfig, t));
CREATE INDEX fd_gist ON fd USING gist (p);
CREATE INDEX fd_brin ON fd USING brin (id);
CREATE TABLE fu (id int, k int, UNIQUE (k) DEFERRABLE INITIALLY DEFERRED, EXCLUDE (id WITH =));
CREATE TABLE fp (id int, v int) PARTITION BY RANGE (id);
CREATE TABLE fp1 PARTITION OF fp FOR VALUES FROM (0) TO (100);
CREATE TABLE fp2 PARTITION OF fp FOR VALUES FROM (100) TO (200);
CREATE INDEX fp_id ON fp (id);
CREATE TABLE fq (id int, w int);
CREATE INDEX fq_id ON fq (id);
INSERT INTO fd (id, a, b, t, p) SELECT i, i % 97, CASE WHEN i % 5 = 0 THEN NULL ELSE i % 13 END, 'T' || i, point(i, i) FROM generate_series(1, 2000) i;
INSERT INTO fu SELECT i, i FROM generate_series(1, 100) i;
INSERT INTO fp SELECT i, i FROM generate_series(0, 199) i;
INSERT INTO fq SELECT i, i FROM generate_series(0, 299) i;
ANALYZE fd; ANALYZE fu; ANALYZE fp; ANALYZE fq;
SET enable_seqscan = off;
SET enable_sort = off;
SET enable_hashjoin = off;
SET enable_nestloop = off;
SET enable_hashagg = off;
-- k: a DESC index scanned forward and backward, NULLS FIRST, a two-column mixed order
SELECT a FROM fd ORDER BY a DESC;
SELECT a FROM fd ORDER BY a;
SELECT b FROM fd ORDER BY b NULLS FIRST;
SELECT a, b FROM fd ORDER BY a DESC NULLS LAST, b NULLS FIRST;
SELECT a, count(*) FROM fd GROUP BY a ORDER BY a;
SELECT DISTINCT b FROM fd ORDER BY b NULLS FIRST;
-- l: expression columns, forward and backward, in a group and a mixed index
SELECT a + b FROM fd ORDER BY a + b;
SELECT a + b FROM fd ORDER BY a + b DESC;
SELECT lower(t), count(*) FROM fd GROUP BY lower(t) ORDER BY lower(t);
SELECT a, b * 2 FROM fd ORDER BY a, b * 2 DESC;
SELECT a, b * 2, t FROM fd ORDER BY a, b * 2 DESC, t;
-- m: the ordered Append over range partitions under a merge join, a window and a sorted group
SELECT id FROM fp ORDER BY id;
SELECT fp.id, fq.w FROM fp JOIN fq ON fp.id = fq.id ORDER BY fp.id;
SELECT id, count(*) OVER (ORDER BY id) FROM fp;
SELECT id, count(*) FROM fp GROUP BY id ORDER BY id;
SELECT id FROM fp WHERE id > 50 ORDER BY id;
-- the write family over the index forms
INSERT INTO fd (id, a, b, t, p) VALUES (5001, 1, 2, 'x', point(0, 0));
UPDATE fd SET b = b + 1 WHERE id = 5001;
DELETE FROM fd WHERE id = 5001;
INSERT INTO fu VALUES (101, 101);
UPDATE fu SET k = k + 1000 WHERE id = 101;
INSERT INTO fp VALUES (150, 1);
-- a user-defined sort operator: the direction is a fact (decision 49, Facts::op_dir over pg_amop)
CREATE FUNCTION fp_lt(int, int) RETURNS bool LANGUAGE sql IMMUTABLE AS 'SELECT $1 < $2';
CREATE FUNCTION fp_gt(int, int) RETURNS bool LANGUAGE sql IMMUTABLE AS 'SELECT $1 > $2';
CREATE OPERATOR <<< (leftarg = int, rightarg = int, function = fp_lt);
CREATE OPERATOR >>> (leftarg = int, rightarg = int, function = fp_gt);
CREATE OPERATOR CLASS fp_user_ops FOR TYPE int USING btree AS OPERATOR 1 <<<, OPERATOR 3 =, OPERATOR 5 >>>, FUNCTION 1 btint4cmp(int, int);
SELECT id FROM fp ORDER BY id USING >>>;
SELECT id FROM fp ORDER BY id USING <<<;
SELECT id, w FROM fq ORDER BY id USING >>>, w;
-- a view target (decompose.md §2.12; lane writes-views): the INSTEAD OF row triggers are a relation fact (`-- trig`),
-- the old row the child's wholerow junk column; without the command's trigger an auto-updatable view is rewritten
-- to its base table (the last UPDATE), a non-updatable one the executor's 55000
CREATE VIEW fv AS SELECT id, a, b FROM fd WHERE id > 0;
CREATE FUNCTION fv_trig() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
CREATE TRIGGER fv_ins INSTEAD OF INSERT ON fv FOR EACH ROW EXECUTE FUNCTION fv_trig();
CREATE TRIGGER fv_upd INSTEAD OF UPDATE ON fv FOR EACH ROW EXECUTE FUNCTION fv_trig();
INSERT INTO fv VALUES (6001, 1, 2);
INSERT INTO fv VALUES (6002, 1, 2) RETURNING old.*, new.*;
UPDATE fv SET b = b + 1 WHERE id = 6001 RETURNING *;
CREATE VIEW fw AS SELECT id, a, b FROM fv WHERE a < 100 WITH CHECK OPTION;
INSERT INTO fw VALUES (6003, 1, 2);
DROP TRIGGER fv_upd ON fv;
UPDATE fv SET b = b + 1 WHERE id = 6001;
-- the AM's whole-relation order (§4.8 `-- rel … sorted=`): a pgrc table declared sorted_by over at most one part gives
-- its bare scan the key's order (the planner's pathkeys, ASC NULLS LAST, lexicographic in declaration order); the
-- decomposer's provenance reads the same fact — a merge join over two sorted scans, an ORDER BY without a Sort, a
-- WITH TIES cut over the sorted scan, and the second key alone (no order: the planner sorts)
CREATE ACCESS METHOD pgrcolumnar TYPE TABLE HANDLER heap_tableam_handler;
CREATE TABLE fs (a int, b int, t text) USING pgrcolumnar WITH (sorted_by = 'a, b');
CREATE TABLE fs2 (a int, v int) USING pgrcolumnar WITH (sorted_by = 'a');
SET enable_hashjoin = off;
SET enable_nestloop = off;
SELECT fs.a, fs.t, fs2.v FROM fs JOIN fs2 ON fs.a = fs2.a;
SELECT a, b FROM fs ORDER BY a, b;
SELECT a FROM fs ORDER BY a FETCH FIRST 2 ROWS WITH TIES;
SELECT b FROM fs ORDER BY b;
RESET enable_hashjoin;
RESET enable_nestloop;
-- a numeric key over a streamed union (V3-R1C, main 09-23): the arms' columns agree on scale and precision (the
-- narrow I64 mantissa key stays), or they differ (the Fp128 normalized key) -- over the open union (a Sort) and
-- through a group's breaker (the arms pushed)
CREATE TABLE fn1 (v numeric(10,2));
CREATE TABLE fn2 (v numeric(10,2));
CREATE TABLE fn3 (v numeric(12,4));
SELECT v FROM (SELECT v FROM fn1 UNION ALL SELECT v FROM fn2) u ORDER BY v;
SELECT v FROM (SELECT v FROM fn1 UNION ALL SELECT v FROM fn3) u ORDER BY v;
SELECT v, count(*) FROM (SELECT v FROM fn1 UNION ALL SELECT v FROM fn2) u GROUP BY v ORDER BY v;
SELECT v, count(*) FROM (SELECT v FROM fn1 UNION ALL SELECT v FROM fn3) u GROUP BY v ORDER BY v;

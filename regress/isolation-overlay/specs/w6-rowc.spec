# W6 V4R-534 (D655 Row C): a one-site UPDATE / DELETE in a data-modifying CTE with RETURNING whose row s1 changed after
# s2's scan read it. PostgreSQL's ExecUpdate / ExecDelete lock the newest version (table_tuple_lock, FIND_LAST_VERSION),
# run EvalPlanQual over it (the scan's quals, the index recheck qual, the SET projection again) and redo the write at the
# newest tid; RETURNING (and so the CTE's reader) sees the row as the retry wrote it, OLD the version it replaced.
# s1 changes a grp = 0 row in each of these ways:
# - its v (a qual-free column the SET reads): EPQ passes, the SET is recomputed over the new v;
# - its grp (the qual): EPQ fails, the row is skipped (not in RETURNING, not written);
# - deleted: skipped;
# - its t (a column the UPDATE does not set, carried): the new t is kept (no lost update);
# - a big external TOAST column's row: the carried value reads back whole after the retry;
# - the index key the scan used: the index recheck qual fails, the row is skipped.
# A VOLATILE SET (nextval) is evaluated again at the recheck (the sequence advances once more), an InitPlan read only
# in RETURNING runs once. RLS: the policy's USING qual rechecked (skip) and its WITH CHECK over the retried row (raise). A CHECK constraint flipped by the newest version
# raises. A stored generated column over a carried column is recomputed from the newest version. Under REPEATABLE READ
# (s3) TM_Updated / TM_Deleted raise 40001. A row s1 moved to another partition makes a direct leaf target's retry raise
# 40001 (the lock's). The EXPLAIN steps pin the plan shapes. Expected output is the C oracle's (PostgreSQL 18, pgdg).

setup
{
 CREATE TABLE rc (id int PRIMARY KEY, grp int, v int, t text);
 INSERT INTO rc SELECT i, i % 3, i, 'row' || i FROM generate_series(1, 12) i;
 CREATE INDEX rc_v ON rc (v);
 CREATE SEQUENCE rcsq;
 CREATE TABLE rcl (id int PRIMARY KEY, grp int, v int, big text);
 ALTER TABLE rcl ALTER big SET STORAGE EXTERNAL;
 INSERT INTO rcl SELECT i, i % 2, i, (SELECT string_agg(md5((g * i)::text), '') FROM generate_series(1, 200) g) FROM generate_series(1, 4) i;
 CREATE TABLE rck (id int PRIMARY KEY, v int CHECK (v < 200));
 INSERT INTO rck SELECT i, i FROM generate_series(1, 4) i;
 CREATE TABLE rcg (id int PRIMARY KEY, a int, b int, g int GENERATED ALWAYS AS (a * 10 + b) STORED);
 INSERT INTO rcg (id, a, b) SELECT i, i, i FROM generate_series(1, 4) i;
 CREATE TABLE rcp (id int, k int, v int) PARTITION BY RANGE (k);
 CREATE TABLE rcp1 PARTITION OF rcp FOR VALUES FROM (0) TO (10);
 CREATE TABLE rcp2 PARTITION OF rcp FOR VALUES FROM (10) TO (20);
 INSERT INTO rcp VALUES (1, 1, 1), (2, 2, 2);
 CREATE TABLE rcr (id int PRIMARY KEY, grp int, v int);
 INSERT INTO rcr SELECT i, 0, i FROM generate_series(1, 4) i;
 ALTER TABLE rcr ENABLE ROW LEVEL SECURITY;
 CREATE POLICY rcr_p ON rcr USING (grp = 0) WITH CHECK (v < 200);
 DROP ROLE IF EXISTS w6_rowc_r;
 CREATE ROLE w6_rowc_r;
 GRANT ALL ON rcr TO w6_rowc_r;
 ANALYZE rc, rcl, rck, rcg, rcr;
}

teardown
{
 DROP TABLE rcr;
 DROP ROLE w6_rowc_r;
 DROP TABLE rcp;
 DROP TABLE rcg;
 DROP TABLE rck;
 DROP TABLE rcl;
 DROP SEQUENCE rcsq;
 DROP TABLE rc;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE rc SET v = v + 100 WHERE id = 3; }
step s1g	{ UPDATE rc SET grp = 2 WHERE id = 6; }
step s1d	{ DELETE FROM rc WHERE id = 9; }
step s1t	{ UPDATE rc SET t = 'new' || id WHERE id = 3; }
step s1big	{ UPDATE rcl SET v = v + 10 WHERE id = 2; }
step s1k	{ UPDATE rck SET v = 150 WHERE id = 2; }
step s1gb	{ UPDATE rcg SET b = b + 5 WHERE id = 2; }
step s1mv	{ UPDATE rcp SET k = 15 WHERE id = 1; }
step s1rk	{ UPDATE rcr SET v = 150 WHERE id = 2; }
step s1rg	{ UPDATE rcr SET grp = 1 WHERE id = 2; }
step s1c	{ COMMIT; }

session s2
step s2b	{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2xu	{ EXPLAIN (COSTS OFF) WITH c AS (UPDATE rc SET v = v + 1 WHERE grp = 0 RETURNING id, grp, v, t) SELECT * FROM c ORDER BY id; }
step s2u	{ WITH c AS (UPDATE rc SET v = v + 1 WHERE grp = 0 RETURNING id, grp, v, t) SELECT * FROM c ORDER BY id; }
step s2uo	{ WITH c AS (UPDATE rc SET v = v + 1 WHERE grp = 0 RETURNING id, old.v AS ov, new.v AS nv, old.t AS ot) SELECT * FROM c ORDER BY id; }
step s2d	{ WITH c AS (DELETE FROM rc WHERE grp = 0 RETURNING id, v, t) SELECT * FROM c ORDER BY id; }
step s2n	{ WITH c AS (UPDATE rc SET v = nextval('rcsq') WHERE grp = 0 RETURNING id, v) SELECT * FROM c ORDER BY id; }
step s2seq	{ SELECT last_value, is_called FROM rcsq; }
step s2initr	{ WITH c AS (UPDATE rc SET v = v + 1 WHERE grp = 0 RETURNING id, v, (SELECT nextval('rcsq')) AS r) SELECT id, v, r FROM c ORDER BY id; }
step s2xi	{ SET LOCAL enable_seqscan = off; SET LOCAL enable_bitmapscan = off; EXPLAIN (COSTS OFF) WITH c AS (UPDATE rc SET grp = grp + 10 WHERE v = 3 RETURNING id, v) SELECT * FROM c; }
step s2i	{ WITH c AS (UPDATE rc SET grp = grp + 10 WHERE v = 3 RETURNING id, v) SELECT * FROM c; }
step s2xbm	{ SET LOCAL enable_seqscan = off; SET LOCAL enable_indexscan = off; EXPLAIN (COSTS OFF) WITH c AS (DELETE FROM rc WHERE id IN (3, 6, 9) RETURNING id, v) SELECT * FROM c ORDER BY id; }
step s2bm	{ WITH c AS (DELETE FROM rc WHERE id IN (3, 6, 9) RETURNING id, v) SELECT * FROM c ORDER BY id; }
step s2big	{ WITH c AS (UPDATE rcl SET v = v + 1 WHERE grp = 0 RETURNING id, v, length(big) AS len, md5(big) AS h) SELECT * FROM c ORDER BY id; }
step s2k	{ WITH c AS (UPDATE rck SET v = v + 100 WHERE id = 2 RETURNING id, v) SELECT * FROM c; }
step s2gb	{ WITH c AS (UPDATE rcg SET a = a + 1 WHERE id = 2 RETURNING id, a, b, g) SELECT * FROM c; }
step s2mv	{ WITH c AS (UPDATE rcp1 SET v = v + 1 WHERE id = 1 RETURNING id, k, v) SELECT * FROM c; }
step s2ct	{ WITH c AS (UPDATE rc SET t = ctid::text || ':' || (rc.*)::text WHERE id = 3 RETURNING id, t) SELECT count(*) FROM c; }
step s2xcw	{ EXPLAIN (COSTS OFF) WITH c AS (UPDATE rc SET t = ctid::text || ' ' || (rc.*)::text WHERE id IN (3, 4) AND ctid <> '(0,0)' RETURNING id, t) SELECT * FROM c ORDER BY id; }
step s2cw	{ WITH c AS (UPDATE rc SET t = ctid::text || ' ' || (rc.*)::text WHERE id IN (3, 4) AND ctid <> '(0,0)' RETURNING id, t) SELECT * FROM c ORDER BY id; }
step s2xcwi	{ SET LOCAL enable_seqscan = off; SET LOCAL enable_bitmapscan = off; EXPLAIN (COSTS OFF) WITH c AS (UPDATE rc SET t = ctid::text || ' ' || (rc.*)::text WHERE id IN (3, 4) AND ctid <> '(0,0)' RETURNING id, t) SELECT * FROM c ORDER BY id; }
step s2cwi	{ SET LOCAL enable_seqscan = off; SET LOCAL enable_bitmapscan = off; WITH c AS (UPDATE rc SET t = ctid::text || ' ' || (rc.*)::text WHERE id IN (3, 4) AND ctid <> '(0,0)' RETURNING id, t) SELECT * FROM c ORDER BY id; }
step s2xcwb	{ SET LOCAL enable_seqscan = off; SET LOCAL enable_indexscan = off; EXPLAIN (COSTS OFF) WITH c AS (UPDATE rc SET t = ctid::text || ' ' || (rc.*)::text WHERE id IN (3, 4) AND ctid <> '(0,0)' RETURNING id, t) SELECT * FROM c ORDER BY id; }
step s2cwb	{ SET LOCAL enable_seqscan = off; SET LOCAL enable_indexscan = off; WITH c AS (UPDATE rc SET t = ctid::text || ' ' || (rc.*)::text WHERE id IN (3, 4) AND ctid <> '(0,0)' RETURNING id, t) SELECT * FROM c ORDER BY id; }
step s2rw	{ WITH c AS (UPDATE rc SET v = v + 1 WHERE grp = 0 RETURNING id, (rc.*)::text AS w) SELECT * FROM c ORDER BY id; }
step s2xr	{ SET LOCAL ROLE w6_rowc_r; EXPLAIN (COSTS OFF) WITH c AS (UPDATE rcr SET v = v + 100 WHERE id = 2 RETURNING id, grp, v) SELECT * FROM c; }
step s2r	{ SET LOCAL ROLE w6_rowc_r; WITH c AS (UPDATE rcr SET v = v + 100 WHERE id = 2 RETURNING id, grp, v) SELECT * FROM c; }
step s2c	{ COMMIT; }

session s3
step s3b	{ BEGIN ISOLATION LEVEL REPEATABLE READ; }
step s3snap	{ SELECT count(*) FROM rc; }
step s3u	{ WITH c AS (UPDATE rc SET v = v + 1 WHERE grp = 0 RETURNING id, v) SELECT * FROM c ORDER BY id; }
step s3d	{ WITH c AS (DELETE FROM rc WHERE grp = 0 RETURNING id) SELECT * FROM c ORDER BY id; }
step s3c	{ COMMIT; }

session s4
step s4sel	{ SELECT id, grp, v, t FROM rc ORDER BY id; }
step s4big	{ SELECT id, v, length(big), md5(big) FROM rcl ORDER BY id; }
step s4rls	{ SELECT id, grp, v FROM rcr ORDER BY id; }
step s4rest	{ SELECT id, v FROM rck ORDER BY id; SELECT id, a, b, g FROM rcg ORDER BY id; SELECT tableoid::regclass, * FROM rcp ORDER BY id; }

# EPQ passes: row 3 updated at its new v (104); RETURNING and the reader see it
permutation s2b s2xu s1v s2u s1c s2c s4sel
# the row left the group: the recheck fails, row 6 is not written and not returned
permutation s2b s1g s2u s1c s2c s4sel
# the row deleted: skipped
permutation s2b s1d s2u s1c s2c s4sel
# a carried column changed: no lost update (t = new3), OLD is the version the retry replaced
permutation s2b s1t s2uo s1c s2c s4sel
permutation s2b s1v s2uo s1c s2c s4sel
# DELETE: the newest version deleted and returned; a failing / deleted row skipped
permutation s2b s1v s2d s1c s2c s4sel
permutation s2b s1g s2d s1c s2c s4sel
permutation s2b s1d s2d s1c s2c s4sel
# a VOLATILE SET runs again at the recheck: 5 calls for 4 rows
permutation s2b s1v s2n s1c s2seq s2c s4sel
# an InitPlan read only in RETURNING runs once, past the recheck (an InitPlan below the write refuses by ROWC_INITPLAN: PG's
# first EvalPlanQual runs it again, and the row executor's fallback does not, D2112)
permutation s2b s1v s2initr s1c s2seq s2c s4sel
# the index key moved: the index recheck qual fails, nothing written
permutation s2b s2xi s1v s2i s1c s2c s4sel
# a bitmap heap scan source: row 3 rechecked (the bitmap's recheck qual passes), 6 deleted, 9 deleted by s1: skipped
permutation s2b s2xbm s1v s1d s2bm s1c s2c s4sel
# a carried external TOAST value reads back whole after the retry
permutation s2b s1big s2big s1c s2c s4big
# a CHECK the newest version flips raises (150 + 100)
permutation s2b s1k s2k s1c s2c s4rest
# a stored generated column over a carried column: recomputed from the newest b
permutation s2b s1gb s2gb s1c s2c s4rest
# a row moved to another partition: the direct leaf's retry raises 40001
permutation s2b s1mv s2mv s1c s2c s4rest
# ctid and the whole row read in the SET are the newest version's
permutation s2b s1t s2ct s1c s2c s4sel
# the same SET with a ctid qual, as e2e m8_rowc_cte c4 serves it (code review r1 C3): the newest version's ctid and whole row
# (t = new3 / v = 103), its ctid qual rechecked; the plan's own scan, an index scan and a bitmap heap scan
permutation s2b s2xcw s1t s2cw s1c s2c s4sel
permutation s2b s2xcwi s1v s2cwi s1c s2c s4sel
permutation s2b s2xcwb s1t s2cwb s1c s2c s4sel
# a whole-row RETURNING after the retry: the row as the retry wrote it
permutation s2b s1t s2rw s1c s2c s4sel
# RLS (a non-superuser role): the policy's USING qual is a scan qual, rechecked over the newest version (fails: skipped);
# its WITH CHECK is checked on the retried row (the newest v 150 + 100 violates it)
permutation s2b s2xr s1rg s2r s1c s2c s4rls
permutation s2b s1rk s2r s1c s2c s4rls
# REPEATABLE READ: TM_Updated / TM_Deleted -> could not serialize access (40001)
permutation s3b s3snap s1v s3u s1c s3c
permutation s3b s3snap s1d s3d s1c s3c

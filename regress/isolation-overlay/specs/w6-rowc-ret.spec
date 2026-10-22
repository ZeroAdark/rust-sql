# W6 V4R-534 re-cut r3 (the 22:48Z audit's combined form): V4R-501's RETURNING old / new system columns in a data-modifying
# CTE's one-site UPDATE / DELETE whose row takes D655 Row C's EvalPlanQual retry (write/rowop_epq.rs). PostgreSQL 18's
# ExecUpdate / ExecDelete lock the newest version, recheck it and redo the write there; ExecProcessReturning's OLD is the
# version the retry replaced (s1's: its ctid, s1's xmin one transaction before s2's, xmax s2's own, the page's header
# after the write), NEW the version the retry wrote. s1 changes a grp = 0 row: its v (EPQ passes), its grp (EPQ fails:
# skipped), deleted (skipped), rolled back (no retry: OLD the original version). A direct leaf of a partitioned table,
# an index scan source and a DELETE retry too. Raw xids stay out of the output: age(), equalities against
# pg_current_xact_id() and between OLD / NEW. The EXPLAIN steps pin the plan shapes. Expected output is the C oracle's
# (PostgreSQL 18, pgdg). D3414: old_by_s1 compares OLD's xmin with the setup's (rr0 / rrp0), not age(old.xmin) = 1,
# which an xid handed out between s1's and s2's writes (autovacuum's ANALYZE) broke; s3x hands out one there.

setup
{
 CREATE TABLE rr (id int PRIMARY KEY, grp int, v int);
 INSERT INTO rr SELECT i, i % 2, i * 10 FROM generate_series(1, 8) i;
 CREATE TABLE rrp (id int, k int, v int) PARTITION BY RANGE (k);
 CREATE TABLE rrp1 PARTITION OF rrp FOR VALUES FROM (0) TO (10);
 CREATE TABLE rrp2 PARTITION OF rrp FOR VALUES FROM (10) TO (20);
 INSERT INTO rrp VALUES (1, 1, 1), (2, 2, 2), (3, 13, 3);
 ANALYZE rr, rrp;
 CREATE TABLE rr0 AS SELECT xmin AS x0 FROM rr WHERE id = 1;
 CREATE TABLE rrp0 AS SELECT xmin AS x0 FROM rrp WHERE id = 1;
}

teardown
{
 DROP TABLE rrp0, rr0;
 DROP TABLE rrp;
 DROP TABLE rr;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE rr SET v = v + 1 WHERE id = 2; }
step s1g	{ UPDATE rr SET grp = 1 WHERE id = 4; }
step s1d	{ DELETE FROM rr WHERE id = 6; }
step s1p	{ UPDATE rrp SET v = v + 100 WHERE id = 2; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
step s2b	{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2xu	{ EXPLAIN (COSTS OFF) WITH c AS (UPDATE rr SET v = v * 2 WHERE grp = 0 RETURNING id, old.ctid AS oc, new.ctid AS nc) SELECT * FROM c ORDER BY id; }
step s2u	{ WITH c AS (UPDATE rr SET v = v * 2 WHERE grp = 0 RETURNING id, old.ctid AS oc, new.ctid AS nc, ctid = new.ctid AS ctid_new, old.v AS ov, new.v AS nv, old.xmin <> (SELECT x0 FROM rr0) AS old_by_s1, old.xmin = new.xmin AS same_xmin, old.xmax = pg_current_xact_id()::xid AS old_xmax_mine, new.xmin = pg_current_xact_id()::xid AS new_xmin_mine, old.cmax AS ocmax, new.cmin AS ncmin, old.tableoid = new.tableoid AS same_rel) SELECT * FROM c ORDER BY id; }
step s2d	{ WITH c AS (DELETE FROM rr WHERE grp = 0 RETURNING id, old.ctid AS oc, ctid = old.ctid AS ctid_old, old.v AS ov, new.v IS NULL AS new_null, new.ctid IS NULL AS new_ctid_null, old.xmin <> (SELECT x0 FROM rr0) AS old_by_s1, old.xmax = pg_current_xact_id()::xid AS old_xmax_mine, old.tableoid::regclass AS orel) SELECT * FROM c ORDER BY id; }
step s2xi	{ SET LOCAL enable_seqscan = off; SET LOCAL enable_bitmapscan = off; EXPLAIN (COSTS OFF) WITH c AS (UPDATE rr SET v = v * 2 WHERE id IN (2, 4) RETURNING id, old.ctid AS oc, new.ctid AS nc) SELECT * FROM c ORDER BY id; }
step s2i	{ SET LOCAL enable_seqscan = off; SET LOCAL enable_bitmapscan = off; WITH c AS (UPDATE rr SET v = v * 2 WHERE id IN (2, 4) RETURNING id, old.ctid AS oc, new.ctid AS nc, old.v AS ov, new.v AS nv, old.xmin <> (SELECT x0 FROM rr0) AS old_by_s1, old.xmax = new.xmin AS xmax_is_new) SELECT * FROM c ORDER BY id; }
step s2p	{ WITH c AS (UPDATE rrp1 SET v = v * 2 WHERE id <= 2 RETURNING id, old.tableoid::regclass AS orel, new.tableoid::regclass AS nrel, old.ctid AS oc, new.ctid AS nc, old.v AS ov, new.v AS nv, old.xmin <> (SELECT x0 FROM rrp0) AS old_by_s1, old.xmax = new.xmin AS xmax_is_new) SELECT * FROM c ORDER BY id; }
step s2c	{ COMMIT; }

session s3
step s3x	{ SELECT pg_current_xact_id() IS NOT NULL AS xid_taken; }

session s4
step s4sel	{ SELECT ctid, id, grp, v FROM rr ORDER BY id; }
step s4p	{ SELECT tableoid::regclass, ctid, id, k, v FROM rrp ORDER BY id; }

# EPQ passes on row 2: OLD is s1's version (its ctid, v 21, s1's xmin, xmax mine), NEW the retry's (v 42)
permutation s2b s2xu s1v s2u s1c s2c s4sel
# D3414: an xid handed out between s1's write and s2's (s3x): OLD is still s1's version
permutation s2b s2xu s1v s3x s2u s1c s2c s4sel
# the row left the group: the recheck fails, row 4 is neither written nor returned
permutation s2b s1g s2u s1c s2c s4sel
# the row deleted: skipped
permutation s2b s1d s2u s1c s2c s4sel
# s1 rolls back: no retry, OLD the original version
permutation s2b s1v s2u s1a s2c s4sel
# DELETE: the newest version deleted, OLD is it (s1's ctid and xmin), NEW NULL; a failing / deleted row skipped
permutation s2b s1v s2d s1c s2c s4sel
permutation s2b s1g s1d s2d s1c s2c s4sel
# an index scan source: row 2 rechecked through the index qual, OLD s1's version
permutation s2b s2xi s1v s2i s1c s2c s4sel
# a direct leaf target: OLD / NEW tableoid the leaf, OLD s1's version
permutation s2b s1p s2p s1c s2c s4p

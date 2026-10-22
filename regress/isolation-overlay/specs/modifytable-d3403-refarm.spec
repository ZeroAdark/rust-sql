# modifytable D3403 REFARM (D3379 isolation umbrella): an UPDATE / DELETE whose target's own scan sits on a hash build
# side probed by the Append of a REFERENCE-marked inheritance (rq) or partitioned (rp) parent's children:
# `UPDATE rt SET b = b + rq.v FROM rq WHERE rt.a = rq.id AND rt.a BETWEEN 40 AND 50`, enable_nestloop / enable_mergejoin
# off, rt small after its filter, so PostgreSQL hashes rt and probes it with each child's rows. A row s1 changed and
# committed while s2 waits on it is rechecked by EvalPlanQual: rt's scan at the newest version, the Hash rebuilt over it,
# the child's row refetched by its junk ctid where the junk tableoid names it (the version the join read).
#  s1b: row 44's b moved: the recheck adds v to the newest b.
#  s1a: row 45 renumbered 450: the newest version fails rt's filter and the hash key, so the row is skipped.
#  s1d: row 46 deleted: skipped.
#  s1v: row 44's b moved and rq's row 44 (child rq1) given v = 100 in the same transaction: the recheck reads the
#  REFERENCE row the join read (v = 1), not the newest one.
#  s1k: row 47's b moved and the parent's own row 47 renumbered 48: the recheck still joins the version the join read.
#  s2w: both parents (rt's Hash probed by rq's union, that join hashed for rp's): each family's row refetched.
# Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
  CREATE TABLE rt (a int PRIMARY KEY, b int) WITH (autovacuum_enabled = off);
  INSERT INTO rt SELECT i, i * 10 FROM generate_series(1, 200) i;
  CREATE TABLE rq (id int, v int) WITH (autovacuum_enabled = off);
  CREATE TABLE rq1 () INHERITS (rq) WITH (autovacuum_enabled = off);
  CREATE TABLE rq2 () INHERITS (rq) WITH (autovacuum_enabled = off);
  INSERT INTO rq VALUES (47, 7);
  INSERT INTO rq1 SELECT i, 1 FROM generate_series(1, 3000) i WHERE i % 2 = 0;
  INSERT INTO rq2 SELECT i, 2 FROM generate_series(1, 3000) i WHERE i % 2 = 1 AND i <> 47;
  CREATE TABLE rp (id int, v int) PARTITION BY RANGE (id);
  CREATE TABLE rp1 PARTITION OF rp FOR VALUES FROM (0) TO (45) WITH (autovacuum_enabled = off);
  CREATE TABLE rp2 PARTITION OF rp FOR VALUES FROM (45) TO (4000) WITH (autovacuum_enabled = off);
  INSERT INTO rp SELECT i, 3 FROM generate_series(1, 3000) i;
  ANALYZE rt, rq, rq1, rq2, rp, rp1, rp2;
}

teardown
{
  DROP TABLE rt, rq1, rq2, rq, rp;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1b	{ UPDATE rt SET b = b + 1000 WHERE a = 44; }
step s1a	{ UPDATE rt SET a = 450 WHERE a = 45; }
step s1d	{ DELETE FROM rt WHERE a = 46; }
step s1v	{ UPDATE rt SET b = b + 1000 WHERE a = 44; UPDATE rq1 SET v = 100 WHERE id = 44; }
step s1k	{ UPDATE rt SET b = b + 1000 WHERE a = 47; UPDATE ONLY rq SET id = 48 WHERE id = 47; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2e	{ EXPLAIN (COSTS OFF) UPDATE rt SET b = b + rq.v FROM rq WHERE rt.a = rq.id AND rt.a BETWEEN 40 AND 50; }
step s2u	{ UPDATE rt SET b = b + rq.v FROM rq WHERE rt.a = rq.id AND rt.a BETWEEN 40 AND 50; }
step s2p	{ UPDATE rt SET b = b + rp.v FROM rp WHERE rt.a = rp.id AND rt.a BETWEEN 40 AND 50; }
step s2w	{ UPDATE rt SET b = b + rq.v + rp.v FROM rq, rp WHERE rt.a = rq.id AND rq.id = rp.id AND rt.a BETWEEN 40 AND 50; }
step s2x	{ DELETE FROM rt USING rq WHERE rt.a = rq.id AND rt.a BETWEEN 40 AND 50; }
step s2r	{ SELECT a, b FROM rt WHERE a BETWEEN 38 AND 52 OR a = 450 ORDER BY a; }

permutation s2e s1c
permutation s1b s2u s1c s2r
permutation s1a s2u s1c s2r
permutation s1d s2u s1c s2r
permutation s1v s2u s1c s2r
permutation s1k s2u s1c s2r
permutation s1b s2p s1c s2r
permutation s1a s2p s1c s2r
permutation s1b s2x s1c s2r
permutation s1k s2x s1c s2r
permutation s1a s2x s1c s2r
permutation s1b s2w s1c s2r
permutation s1v s2w s1c s2r
permutation s1a s2w s1c s2r

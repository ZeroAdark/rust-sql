# lockrows queue-0012: FOR UPDATE of a subquery that PostgreSQL plans apart (a LIMIT keeps it from being pulled up). The
# locking clause goes into the subquery (Limit -> LockRows -> Sort -> Seq Scan); the outer LockRows has only the
# subquery's COPY mark (and a REFERENCE mark for a joined table FOR UPDATE OF does not name), so it locks nothing and
# never starts EvalPlanQual. When s1 changed a row (READ COMMITTED), the subquery's own recheck reruns Sort -> scan over
# the newest version: the row keeps its sorted place with its new values; a row whose newest version fails the qual is
# skipped and the LIMIT takes the next one; a deleted row is skipped; SKIP LOCKED skips a row s1 holds; NOWAIT raises
# 55P03. The rows pass the outer LockRows unchanged. s3 joins the subquery to lqw (FOR UPDATE OF s: lqw's mark is
# REFERENCE, never locked) by a hash join that builds on the subquery, read in index order (no Sort below its lock: the
# lock's pipeline produces the build). D3379 FEEDABOVE (s4): the Sort form of that join as a nested loop over the
# subquery, its inner a Materialize of lqw (no index scan): the recheck reruns Sort -> scan over the newest version and the
# rechecked row goes on through the Limit and the join. D3379 OPABOVE (s5): a filter above the subquery's LockRows (the
# Subquery Scan's WHERE over an index-ordered LIMIT / OFFSET): a row whose newest version fails the subquery's qual is
# skipped and the LIMIT takes the next, the filter over the rows the Limit hands on. Expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE lqj (id int PRIMARY KEY, state text NOT NULL, prio int NOT NULL, v text NOT NULL, wid int);
  INSERT INTO lqj SELECT i, CASE WHEN i % 3 = 0 THEN 'done' ELSE 'ready' END, 100 - i, 'v' || i, 1 + i % 4 FROM generate_series(1, 24) i;
  CREATE INDEX lqj_state_prio ON lqj (state, prio, id);
  CREATE TABLE lqw (id int PRIMARY KEY, name text NOT NULL);
  INSERT INTO lqw SELECT i, 'w' || i FROM generate_series(1, 4) i;
  ANALYZE lqj, lqw;
}
teardown { DROP TABLE lqj, lqw; }

session s1
setup		{ BEGIN; }
step s1v	{ UPDATE lqj SET v = v || '+' WHERE id = 23; }
step s1s	{ UPDATE lqj SET state = 'done' WHERE id = 23; }
step s1d	{ DELETE FROM lqj WHERE id = 23; }
step s1w	{ UPDATE lqw SET name = name || '+' WHERE id = 4; }
step s1l	{ SELECT id FROM lqj WHERE id = 23 FOR UPDATE; }
step s1v4	{ UPDATE lqj SET v = v || '+' WHERE id = 4; }
step s1s4	{ UPDATE lqj SET state = 'done' WHERE id = 4; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
setup		{ SET enable_indexscan = off; SET enable_bitmapscan = off; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT * FROM (SELECT id, v FROM lqj WHERE state = 'ready' ORDER BY prio, id LIMIT 3) s FOR UPDATE; }
step s2l	{ SELECT * FROM (SELECT id, v FROM lqj WHERE state = 'ready' ORDER BY prio, id LIMIT 3) s FOR UPDATE; }
step s2sk	{ SELECT * FROM (SELECT id, v FROM lqj WHERE state = 'ready' ORDER BY prio, id LIMIT 2) s FOR UPDATE SKIP LOCKED; }
step s2nw	{ SELECT * FROM (SELECT id, v FROM lqj WHERE state = 'ready' ORDER BY prio, id LIMIT 1) s FOR UPDATE NOWAIT; }

session s3
setup		{ SET enable_seqscan = off; SET enable_bitmapscan = off; SET enable_nestloop = off; SET enable_mergejoin = off; }
step s3x	{ EXPLAIN (COSTS OFF) SELECT s.id, s.v, w.name FROM (SELECT id, v, wid FROM lqj WHERE state = 'ready' ORDER BY prio, id LIMIT 3) s JOIN lqw w ON w.id = s.wid FOR UPDATE OF s; }
step s3j	{ SELECT s.id, s.v, w.name FROM (SELECT id, v, wid FROM lqj WHERE state = 'ready' ORDER BY prio, id LIMIT 3) s JOIN lqw w ON w.id = s.wid FOR UPDATE OF s; }

session s4
setup		{ SET enable_indexscan = off; SET enable_bitmapscan = off; SET enable_hashjoin = off; SET enable_mergejoin = off; }
step s4x	{ EXPLAIN (COSTS OFF) SELECT s.id, s.v, w.name FROM (SELECT id, v, wid FROM lqj WHERE state = 'ready' ORDER BY prio, id LIMIT 3) s JOIN lqw w ON w.id = s.wid FOR UPDATE OF s; }
step s4n	{ SELECT s.id, s.v, w.name FROM (SELECT id, v, wid FROM lqj WHERE state = 'ready' ORDER BY prio, id LIMIT 3) s JOIN lqw w ON w.id = s.wid FOR UPDATE OF s; }

session s5
setup		{ SET enable_seqscan = off; SET enable_bitmapscan = off; }
step s5x	{ EXPLAIN (COSTS OFF) SELECT * FROM (SELECT id, v FROM lqj WHERE state = 'ready' ORDER BY id LIMIT 6 OFFSET 1) s WHERE s.id % 2 = 0 FOR UPDATE; }
step s5f	{ SELECT * FROM (SELECT id, v FROM lqj WHERE state = 'ready' ORDER BY id LIMIT 6 OFFSET 1) s WHERE s.id % 2 = 0 FOR UPDATE; }

# state = 'ready' by prio: 23 (77), 22 (78), 20 (80), 19 (81), ... (every third id is done)
permutation s2x s1v s2l s1c
permutation s1s s2l s1c
permutation s1d s2l s1c
permutation s1l s2sk s1c
permutation s1l s2nw s1c
permutation s3x s1v s3j s1c
permutation s1w s3j s1c
permutation s4x s1v s4n s1c
permutation s1s s4n s1c
permutation s5x s1v4 s5f s1c
permutation s1s4 s5f s1c

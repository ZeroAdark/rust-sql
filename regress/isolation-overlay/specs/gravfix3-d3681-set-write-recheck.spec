# D3681: an UPDATE / DELETE through a partitioned root whose eight leaves are twins reads them as ONE relation-set pipeline
# (pgrust.gravity_relset = on, decompose ops/append.rs `relsets`), each row written to its own leaf (the window's relation:
# lowerv4 scan/heap.rs `set_rel`). Another session changes one row of leaf e5 first (READ COMMITTED), so s2's write waits on
# it, then EvalPlanQual rechecks the newest version: the recheck node binds that row's own leaf (hosts heap_source.rs) and
# reruns the quals; the retry writes that leaf.
#  (a) s1 raises the row's v: s2's UPDATE (and DELETE) ... WHERE v > 10 rechecks and writes the newest version
#  (b) s1 sets the row's v to 0: the recheck fails the qual, the row is skipped, every other row written
#  (c) s1 deletes the row: s2 skips it
#  (d) s1 aborts (control)
#  (e) while s2 waits on the row, s3 lists s2's relation locks (pg_locks): the plan's own (the root, each leaf, each leaf
#      index), no other lock and no other mode
#  (f) s1 holds leaf e3 locked: s4's UPDATE (its second run, the program kept) waits on e3 with the root and e0 .. e2 locked,
#      PostgreSQL's planner order
#  (g) the per-leaf counts of the statistics views after (a): n_tup_upd, n_tup_hot_upd, n_tup_del (s1 and s2 flush theirs)
#  (h) (a) with pgrust.gravity_relset = off (one arm per leaf): the same answers
# The statistics make every write's plan an Append of sequential scans (the set's form). Expected output is PostgreSQL
# 18.6's (the C oracle).
setup
{
 CREATE TABLE d3681e (id int, k int, v int, w int) PARTITION BY LIST (k);
 CREATE INDEX d3681e_v ON d3681e (v);
 DO $$ BEGIN FOR i IN 0..7 LOOP EXECUTE format('CREATE TABLE d3681e%s PARTITION OF d3681e FOR VALUES IN (%s)', i, i); END LOOP; END $$;
 INSERT INTO d3681e SELECT g, g % 8, g % 50, 0 FROM generate_series(1, 400) g;
 ANALYZE d3681e;
}

teardown
{
 DROP TABLE d3681e;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1u	{ UPDATE d3681e SET v = v + 100 WHERE id = 13; }
step s1z	{ UPDATE d3681e SET v = 0 WHERE id = 13; }
step s1d	{ DELETE FROM d3681e WHERE id = 13; }
step s1x	{ LOCK TABLE d3681e3 IN EXCLUSIVE MODE; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }
step s1f	{ SELECT pg_stat_force_next_flush(); }

session s2
setup		{ SET application_name = 'd3681s2'; SET max_parallel_workers_per_gather = 0; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2o	{ SET pgrust.gravity_relset = off; }
step s2u	{ UPDATE d3681e SET w = w + 1 WHERE v > 10; }
step s2d	{ DELETE FROM d3681e WHERE v > 10; }
step s2c	{ COMMIT; }
step s2f	{ SELECT pg_stat_force_next_flush(); }

session s3
step s3r	{ SELECT tableoid::regclass, count(*), sum(v), sum(w) FROM d3681e GROUP BY 1 ORDER BY 1; }
step s3l	{ SELECT c.relname, l.mode, l.granted FROM pg_locks l JOIN pg_class c ON c.oid = l.relation WHERE l.locktype = 'relation' AND l.pid = (SELECT pid FROM pg_stat_activity WHERE application_name = 'd3681s2') ORDER BY c.relname COLLATE "C", l.mode; }
step s3m	{ SELECT c.relname, l.mode, l.granted FROM pg_locks l JOIN pg_class c ON c.oid = l.relation WHERE l.locktype = 'relation' AND l.pid = (SELECT pid FROM pg_stat_activity WHERE application_name = 'd3681s4') ORDER BY c.relname COLLATE "C", l.mode; }
step s3s	{ SELECT relname, n_tup_upd, n_tup_hot_upd, n_tup_del FROM pg_stat_user_tables WHERE relname LIKE 'd3681e%' ORDER BY relname COLLATE "C"; }

session s4
setup		{ SET application_name = 'd3681s4'; SET max_parallel_workers_per_gather = 0; }
step s4u	{ UPDATE d3681e SET w = w + 1 WHERE v > 10; }

permutation s1u s2u s3l s1c s2c s3r
permutation s1z s2u s1c s2c s3r
permutation s1d s2u s1c s2c s3r
permutation s1u s2u s1a s2c s3r
permutation s1u s2d s1c s2c s3r
permutation s1z s2d s1c s2c s3r
permutation s1d s2d s1c s2c s3r
permutation s4u s1x s4u s3m s1c s3r
permutation s1u s2u s1c s2c s1f s2f s3s
permutation s2o s1u s2u s1c s2c s3r

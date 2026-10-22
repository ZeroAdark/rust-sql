# lockrows INCSTREAM (lk-0038 / lk-0044 / lk-0050): a LIMIT over LockRows over an Incremental Sort over an index scan,
# whose sort producer the lock pipeline pulls as it reads (lowerv4 ops/sort/inc_stream.rs: each pull closes the LIMIT's
# count of rows, here 3, at a prefix group's close). Rows the lock passes over make it pull again, as PostgreSQL's Limit
# pulls past them: rows another transaction holds (SKIP LOCKED), rows a committed update moved out of the quals or deleted
# (the recheck fails), and an OFFSET. A row that still qualifies is returned in its newest version; NOWAIT raises on a
# held row. The first claimable rows in (run_at, id) order are 190, 163, 109, 82, 55, 1, 191.
# INCAGG (D3409): the same lock in a subquery an aggregate reads (its pipeline produces for the aggregate's): the rows the
# recheck drops make the lock pull again, and the aggregate counts the rows that passed in their newest version.

setup
{
 CREATE TABLE job (id int PRIMARY KEY, queue text NOT NULL, prio int NOT NULL, state text NOT NULL, run_at timestamptz NOT NULL, note text);
 CREATE INDEX job_run_at ON job (run_at);
 INSERT INTO job SELECT i, CASE WHEN i % 3 = 0 THEN 'sms' ELSE 'mail' END, i % 5, CASE WHEN i % 4 = 0 THEN 'done' ELSE 'ready' END, timestamptz '2024-01-01 00:00+00' + ((i * 37) % 1000) * interval '1 minute', 'n' || i FROM generate_series(1, 200) i;
 ANALYZE job;
}

teardown
{
 DROP TABLE job;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1h	{ SELECT id FROM job WHERE id IN (190, 163, 109, 82) ORDER BY id FOR UPDATE; }
step s1d	{ UPDATE job SET state = 'done' WHERE id IN (190, 163, 109); }
step s1x	{ DELETE FROM job WHERE id IN (190, 109, 55); }
step s1n	{ UPDATE job SET note = 'held' WHERE id IN (190, 163); }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; SET enable_seqscan = off; }
step s2e	{ EXPLAIN (COSTS OFF) SELECT id FROM job WHERE state = 'ready' AND queue = 'mail' ORDER BY run_at, id LIMIT 3 FOR UPDATE; }
step s2u	{ SELECT id, run_at, state, note FROM job WHERE state = 'ready' AND queue = 'mail' ORDER BY run_at, id LIMIT 3 FOR UPDATE; }
step s2k	{ SELECT id, run_at FROM job WHERE state = 'ready' AND queue = 'mail' ORDER BY run_at, id LIMIT 3 FOR UPDATE SKIP LOCKED; }
step s2o	{ SELECT id, run_at FROM job WHERE state = 'ready' AND queue = 'mail' ORDER BY run_at, id LIMIT 2 OFFSET 2 FOR UPDATE; }
step s2w	{ SELECT id FROM job WHERE state = 'ready' AND queue = 'mail' ORDER BY run_at, id LIMIT 3 FOR UPDATE NOWAIT; }
step s2ae	{ EXPLAIN (COSTS OFF) SELECT count(*), sum(id), max(note) FROM (SELECT id, note FROM job WHERE state = 'ready' AND queue = 'mail' ORDER BY run_at, id LIMIT 3 FOR UPDATE) s; }
step s2a	{ SELECT count(*), sum(id), max(note) FROM (SELECT id, note FROM job WHERE state = 'ready' AND queue = 'mail' ORDER BY run_at, id LIMIT 3 FOR UPDATE) s; }
step s2c	{ COMMIT; }

permutation s2e s1c s2c
permutation s1h s2k s1c s2c
permutation s1d s2u s1c s2c
permutation s1x s2u s1c s2c
permutation s1n s2u s1c s2c
permutation s1d s2o s1c s2c
permutation s1h s2w s1c s2c
permutation s2ae s1c s2c
permutation s1d s2a s1c s2c
permutation s1x s2a s1c s2c
permutation s1n s2a s1c s2c

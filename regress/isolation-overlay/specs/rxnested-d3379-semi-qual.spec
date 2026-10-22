# D3379 SEMI-QUAL: a data-modifying CTE's DELETE semi-joined to another writing CTE's rows (a Hash Semi Join over the
# CTE Scan, a COPY mark) with a join qual past the hash keys, while another session changes the target rows (READ
# COMMITTED). EvalPlanQual tests the target's newest version against the join's first match, the CTE row the COPY mark
# stored. With several CTE rows on one key (s2dup), which real match comes first is the plan's choice; every change here
# keeps or fails every real match alike, so each row's outcome shows only that the tested row is a real match (its keys
# and its qual): id 4 and 14 still match and are deleted (14's new value fails the rows that never matched), 24 fails
# every row and is skipped, 34 is gone, 44 moves to a key no CTE row has and is skipped (the keys are tested too).
# s2uniq is modifytable_rowc_copy's c3b form, one CTE row per key; 57 moves to another key the same way.
# Expected output is PostgreSQL 18.6's (the C oracle).

setup
{
 CREATE TABLE sqb (id int PRIMARY KEY, grp int NOT NULL, amt int NOT NULL);
 CREATE TABLE sqs (id int PRIMARY KEY, v int NOT NULL);
 INSERT INTO sqb SELECT g, g % 10, g FROM generate_series(1, 500) g;
 INSERT INTO sqs SELECT g, 0 FROM generate_series(1, 2000) g;
 UPDATE sqs SET v = 200 WHERE id = 14;
 ANALYZE sqb, sqs;
}

teardown
{
 DROP TABLE sqb, sqs;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1keep	{ UPDATE sqs SET v = 1 WHERE id = 4; }
step s1near	{ UPDATE sqs SET v = 190 WHERE id = 14; }
step s1fail	{ UPDATE sqs SET v = 1000 WHERE id = 24; }
step s1del	{ DELETE FROM sqs WHERE id = 34; }
step s1key	{ UPDATE sqs SET id = 3044 WHERE id = 44; }
step s1u	{ UPDATE sqs SET v = 3 WHERE id IN (7, 17); }
step s1ufail	{ UPDATE sqs SET v = 900 WHERE id = 27; }
step s1ukey	{ UPDATE sqs SET id = 3057 WHERE id = 57; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; SET max_parallel_workers_per_gather = 0; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2dup	{ WITH u AS (UPDATE sqb SET amt = amt + 1 WHERE grp = 4 RETURNING id % 50 AS k, amt), d AS (DELETE FROM sqs t WHERE EXISTS (SELECT 1 FROM u WHERE u.k = t.id AND u.amt > t.v) RETURNING t.id, t.v) SELECT id, v FROM d ORDER BY id; }
step s2uniq	{ WITH u AS (UPDATE sqb SET amt = amt + 1 WHERE grp = 7 RETURNING id, amt), d AS (DELETE FROM sqs t WHERE EXISTS (SELECT 1 FROM u WHERE u.id = t.id AND u.amt > t.v) RETURNING t.id, t.v) SELECT count(*), sum(id), sum(v) FROM d; }
step s2exp	{ EXPLAIN (COSTS OFF) WITH u AS (UPDATE sqb SET amt = amt + 1 WHERE grp = 4 RETURNING id % 50 AS k, amt), d AS (DELETE FROM sqs t WHERE EXISTS (SELECT 1 FROM u WHERE u.k = t.id AND u.amt > t.v) RETURNING t.id, t.v) SELECT id, v FROM d ORDER BY id; }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT id, v FROM sqs WHERE id IN (4, 7, 14, 17, 24, 27, 34, 44, 57, 3044, 3057) ORDER BY id; }

permutation s2exp s1keep s1near s1fail s1del s1key s2dup s1c s2c s3sel
permutation s1u s1ufail s1ukey s2uniq s1c s2c s3sel

# producerchain C: an UPDATE / DELETE whose target's own scan is the outer of a chain of nested loops, INNER or LEFT,
# each over a Material of another relation's heap scan (a view rule's `UPDATE t ... WHERE t.k = OLD.k` over the view's
# LEFT join, rules#364: PostgreSQL 18.6 joins the target first when every relation is filtered on the rule's key, or
# when no hash or merge join is taken), meets a target row s1 changed. EvalPlanQual reruns the plan with the target's
# newest version and every other relation's row refetched at its junk ctid, the version the statement joined (a NULL
# one, a LEFT join's unmatched inner, is no row: that row stays NULL-extended): a version that still passes its filters
# and the join filters is written (SET over its new values), one whose key moved off the joined row or that was deleted
# is skipped, a target row joined twice is written once, and s1's changes to the other relations are not seen.

setup
{
 CREATE TABLE mp (pid int, txt text);
 INSERT INTO mp VALUES (1, 'p1'), (2, 'p2'), (5, 'p5'), (6, 'p6'), (8, 'p8');
 CREATE TABLE mc (pid int, descrip text, n int NOT NULL DEFAULT 0);
 INSERT INTO mc VALUES (1, 'c1', 0), (2, 'c2', 0), (5, 'c5', 0), (5, 'c5b', 0), (6, 'c6', 0), (8, 'c8', 0);
 CREATE TABLE mq (pid int, tag text);
 INSERT INTO mq VALUES (1, 'q1'), (5, 'q5'), (6, 'q6');
 CREATE VIEW mv AS SELECT mp.pid, txt, descrip FROM mp LEFT JOIN mc USING (pid);
 CREATE RULE mv_upd AS ON UPDATE TO mv DO INSTEAD
   UPDATE mc SET descrip = new.descrip, n = n + 1 WHERE mc.pid = old.pid;
}

teardown
{
 DROP VIEW mv;
 DROP TABLE mp;
 DROP TABLE mc;
 DROP TABLE mq;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1n	{ UPDATE mc SET n = n + 100 WHERE descrip = 'c6'; }
step s1k	{ UPDATE mc SET pid = 7 WHERE descrip = 'c5'; }
step s1d	{ DELETE FROM mc WHERE descrip = 'c2'; }
step s1p	{ UPDATE mp SET txt = 'moved' WHERE pid = 8; }
step s1q	{ INSERT INTO mq VALUES (8, 'q8'); }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET max_parallel_workers_per_gather = 0; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2r	{ UPDATE mv SET descrip = 'r' WHERE pid = 5; }
step s2v	{ UPDATE mv SET descrip = 'v' WHERE txt IN ('p2', 'p5', 'p6', 'p8'); }
step s2u	{ UPDATE mc SET n = mc.n + 1 FROM mp LEFT JOIN mq USING (pid) WHERE mc.pid = mp.pid AND mp.txt IN ('p2', 'p5', 'p6', 'p8') RETURNING mc.pid, mc.descrip, mc.n, mq.tag; }
step s2d	{ DELETE FROM mc USING mp LEFT JOIN mq USING (pid) WHERE mc.pid = mp.pid AND mp.txt IN ('p2', 'p5', 'p6', 'p8') RETURNING mc.pid, mc.descrip, mq.tag; }
step s2c	{ COMMIT; }

session s3
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET max_parallel_workers_per_gather = 0; }
step s3sel	{ SELECT pid, descrip, n FROM mc ORDER BY pid, descrip; }
step s3pr	{ EXPLAIN (COSTS OFF) UPDATE mv SET descrip = 'r' WHERE pid = 5; }
step s3pv	{ EXPLAIN (COSTS OFF) UPDATE mv SET descrip = 'v' WHERE txt IN ('p2', 'p5', 'p6', 'p8'); }
step s3pu	{ EXPLAIN (COSTS OFF) UPDATE mc SET n = mc.n + 1 FROM mp LEFT JOIN mq USING (pid) WHERE mc.pid = mp.pid AND mp.txt IN ('p2', 'p5', 'p6', 'p8'); }
step s3pd	{ EXPLAIN (COSTS OFF) DELETE FROM mc USING mp LEFT JOIN mq USING (pid) WHERE mc.pid = mp.pid AND mp.txt IN ('p2', 'p5', 'p6', 'p8'); }

permutation s3pr s3pv s3pu s3pd s1c s2c
permutation s1k s2r s1c s2c s3sel
permutation s1n s2r s1c s2c s3sel
permutation s1n s1k s1d s1p s2v s1c s2c s3sel
permutation s1n s1k s1d s1q s2u s1c s2c s3sel
permutation s1n s1k s1p s1q s2d s1c s2c s3sel
permutation s2v s1n s2c s1c s3sel

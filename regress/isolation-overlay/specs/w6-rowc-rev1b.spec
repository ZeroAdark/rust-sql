# W6-D655-ROWC (V4R-534) kit, from code review round 1 (correctness lens, 21:1xZ): a newly served UPDATE / DELETE CTE beside an
# outer FOR UPDATE whose row s1 changed (the lock rechecks): the CTE must write once (no rerun of its pipeline).

setup
{
 CREATE TABLE rw (id int PRIMARY KEY, v int);
 INSERT INTO rw SELECT i, i FROM generate_series(1, 4) i;
 CREATE TABLE rl (id int PRIMARY KEY, v int);
 INSERT INTO rl SELECT i, i FROM generate_series(1, 4) i;
 CREATE SEQUENCE rlsq;
 ANALYZE rw, rl;
}

teardown
{
 DROP TABLE rw;
 DROP TABLE rl;
 DROP SEQUENCE rlsq;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1l	{ UPDATE rl SET v = v + 100 WHERE id = 1; }
step s1w	{ UPDATE rw SET v = v + 100 WHERE id = 1; }
step s1c	{ COMMIT; }

session s2
step s2b	{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2lk	{ WITH u AS (UPDATE rw SET v = v + 1 WHERE id <= 2 RETURNING id, v, nextval('rlsq') AS n) SELECT id, v FROM rl WHERE id <= 2 ORDER BY id FOR UPDATE; }
step s2lk2	{ WITH u AS (UPDATE rw SET v = v + 1 WHERE id <= 2 RETURNING id, v, nextval('rlsq') AS n) SELECT id, v FROM rl WHERE id <= 2 FOR UPDATE; }
step s2lkd	{ WITH d AS (DELETE FROM rw WHERE id = 2 RETURNING id, nextval('rlsq') AS n) SELECT id, v FROM rl WHERE id <= 2 ORDER BY id FOR UPDATE; }
step s2lkl	{ WITH u AS (UPDATE rw SET v = v + 1 WHERE id <= 2 RETURNING id, v, nextval('rlsq') AS n) SELECT id, v FROM rl WHERE id <= 3 ORDER BY id LIMIT 1 FOR UPDATE; }
step s2c	{ COMMIT; }

session s4
step s4sel	{ SELECT 'rw' AS t, id, v FROM rw UNION ALL SELECT 'rl', id, v FROM rl ORDER BY 1, 2; }
step s4seq	{ SELECT last_value, is_called FROM rlsq; }

permutation s2b s1l s2lk s1c s2c s4sel s4seq
permutation s2b s1l s2lk2 s1c s2c s4sel s4seq
permutation s2b s1l s2lkd s1c s2c s4sel s4seq
permutation s2b s1l s2lkl s1c s2c s4sel s4seq
permutation s2b s1w s2lk s1c s2c s4sel s4seq

# W5 multilock Row A (lowerv4 write/lock_rerun.rs), review round 2 S3 (D2050's mechanism): the SECOND and later
# EvalPlanQual rechecks of one statement over a cost-path outer-first hash join. PG keeps the EPQ plan tree across
# rechecks: ExecReScanHashJoin destroys the table (epqParam changed) but keeps hj_OuterNotEmpty (true after a recheck
# whose outer had a row), so the next recheck builds FIRST and, over an empty build (the locked build row's newest version
# fails its qual), never runs its outer side: the outer qual that may raise (s2r) or calls a VOLATILE function (s2v,
# s2va) runs only where PG runs it (the epq probe's record, D2674), and the same rechecks with a quiet outer qual (s2q).
# Over two outer-first joins (s2xr, s2xv) the lower build that may raise or calls a VOLATILE function runs only once
# the upper join pulls its outer side (lockrows rerun.rs `first_held`, D2033). Expected output is the C oracle's
# (PostgreSQL 18.6, pgdg).

setup
{
  CREATE TABLE fa (id int PRIMARY KEY, v int NOT NULL);
  INSERT INTO fa SELECT i, i FROM generate_series(1, 5) i;
  CREATE TABLE fb (id int PRIMARY KEY, a_id int NOT NULL, w int NOT NULL);
  INSERT INTO fb SELECT i, (i - 1) % 5 + 1, i * 10 FROM generate_series(1, 15) i;
  CREATE TABLE xc (k int PRIMARY KEY, x int NOT NULL);
  INSERT INTO xc VALUES (1, 1), (2, 2);
  CREATE TABLE xa (id int PRIMARY KEY, v int NOT NULL);
  INSERT INTO xa SELECT i, i FROM generate_series(1, 60) i;
  CREATE TABLE xb (id int PRIMARY KEY, a_id int NOT NULL, c_id int NOT NULL);
  INSERT INTO xb SELECT i, CASE WHEN i <= 5 THEN i ELSE 100 END, CASE WHEN i = 3 THEN 2 ELSE 1 END FROM generate_series(1, 1000) i;
  CREATE SEQUENCE fcnt;
  CREATE FUNCTION ftick(int) RETURNS bool LANGUAGE plpgsql VOLATILE AS $$ BEGIN PERFORM nextval('fcnt'); RETURN $1 >= 0; END $$;
  ANALYZE fa, fb, xa, xb, xc;
}

teardown
{
  DROP TABLE fa, fb, xa, xb, xc;
  DROP FUNCTION ftick(int);
  DROP SEQUENCE fcnt;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1u	{ UPDATE fb SET w = w WHERE id = 1; UPDATE fa SET v = -1 WHERE id = 3; UPDATE fb SET w = 999 WHERE id = 8; }
step s1x	{ UPDATE xb SET c_id = c_id WHERE id = 1; UPDATE xa SET v = -1 WHERE id = 3; UPDATE xc SET x = 999 WHERE k = 2; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; SET join_collapse_limit = 1; }
step s2b	{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2e	{ EXPLAIN (COSTS OFF) SELECT a.id, b.id FROM fa a JOIN fb b ON b.a_id = a.id WHERE a.v > 0 AND 100 / (b.w - 999) < 1000 FOR UPDATE; }
step s2r	{ SELECT a.id, b.id FROM fa a JOIN fb b ON b.a_id = a.id WHERE a.v > 0 AND 100 / (b.w - 999) < 1000 FOR UPDATE; }
step s2v	{ SELECT a.id, b.id FROM fa a JOIN fb b ON b.a_id = a.id WHERE a.v > 0 AND ftick(b.w) FOR UPDATE; }
step s2va	{ SELECT a.id, b.id FROM fa a JOIN fb b ON b.a_id = a.id WHERE a.v > 0 AND ftick(b.w) FOR UPDATE OF a; }
step s2q	{ SELECT a.id, b.id, b.w FROM fa a JOIN fb b ON b.a_id = a.id WHERE a.v > 0 AND b.w <> 12345 FOR UPDATE; }
step s2xe	{ EXPLAIN (COSTS OFF) SELECT b.id, c.k, a.id FROM xb b JOIN xc c ON c.k = b.c_id JOIN xa a ON a.id = b.a_id WHERE 100 / (c.x - 999) < 1000 AND a.v > 0 FOR UPDATE; }
step s2xr	{ SELECT b.id, c.k, a.id FROM xb b JOIN xc c ON c.k = b.c_id JOIN xa a ON a.id = b.a_id WHERE 100 / (c.x - 999) < 1000 AND a.v > 0 FOR UPDATE; }
step s2xv	{ SELECT b.id, c.k, a.id FROM xb b JOIN xc c ON c.k = b.c_id JOIN xa a ON a.id = b.a_id WHERE ftick(c.x) AND a.v > 0 FOR UPDATE; }
step s2last	{ SELECT last_value, is_called FROM fcnt; }
step s2c	{ COMMIT; }

# both marks locked: recheck 1 (b1 traversed) fetches the outer first; rechecks 2-4 (a3's new version fails a.v > 0: an
# empty build) build first in PG and never evaluate the outer's qual, which raises on b8's new version
permutation s2b s2e s1u s2r s1c s2c
# the same with a VOLATILE outer qual: PG calls ftick 15 + 1 times
permutation s2b s1u s2v s1c s2last s2c
# FOR UPDATE OF a alone, the build side: rechecks 2 and 3 (b8, b13) build first in PG
permutation s2b s1u s2va s1c s2last s2c
# a quiet outer qual (served): the same rechecks, the rows PG returns
permutation s2b s1u s2q s1c s2c
# two outer-first joins over xb: recheck 1 (b1) runs HJ0's build over xc; recheck 2 (b3: c2 and a3 traversed) builds xa
# first, empty: PG never builds xc
permutation s2b s2xe s1x s2xr s1c s2c
permutation s2b s1x s2xv s1c s2last s2c

# lockrows CTECOPY: the statement's own UPDATE / DELETE beside a writing CTE whose rows it joins (the CTE's RETURNING
# read through a CteScan). PostgreSQL marks the CTE's range table entry ROW_MARK_COPY and carries its row whole (the
# junk u.*); when the statement's write meets a concurrently updated target row, EvalPlanQual reruns the join over that
# row's newest version and the CTE row as stored (EvalPlanQualFetchRowMark's copy leg): the CTE never runs again. Forms:
# a semi join (pid IN (SELECT id FROM u)) and an inner join (FROM u, reading a CTE column), both hashed (a nested loop
# over the target's index is a write target without a single producer chain), the target on the build side with the
# CTE scan probing, an UPDATE and a DELETE (an inheritance family's too), RETURNING the CTE row's columns, an UPDATE and
# an INSERT CTE; a concurrent update kept in the join (the value moves), one moving the join key to another CTE row's
# key (the stored row does not match it: skipped), a concurrent delete, and a concurrent update of the CTE's own row
# (the CTE's write rechecks it). Expected output is the C oracle's (PostgreSQL 18, pgdg).
setup
{
  CREATE TABLE cp (id int PRIMARY KEY, grp int NOT NULL, w int NOT NULL);
  INSERT INTO cp SELECT g, g % 5, 0 FROM generate_series(1, 50) g;
  CREATE TABLE cc (id int PRIMARY KEY, pid int, val int NOT NULL);
  INSERT INTO cc SELECT g, (g % 50) + 1, g FROM generate_series(1, 400) g;
  CREATE TABLE cl (id int, note text);
  CREATE TABLE cq (id int PRIMARY KEY, w int NOT NULL);
  INSERT INTO cq SELECT g, 0 FROM generate_series(1, 5000) g;
  CREATE TABLE ch (id int, v int);
  CREATE TABLE ch1 () INHERITS (ch);
  CREATE TABLE ch2 () INHERITS (ch);
  INSERT INTO ch SELECT g, g FROM generate_series(1, 1000) g;
  INSERT INTO ch1 SELECT g, g FROM generate_series(1001, 2000) g;
  INSERT INTO ch2 SELECT g, g FROM generate_series(2001, 3000) g;
  ANALYZE cp, cc, cl, cq, ch, ch1, ch2;
}

teardown
{
  DROP TABLE cp, cc, cl, cq;
  DROP TABLE ch CASCADE;
}

session s1
step s1b	{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE cc SET val = val + 1000 WHERE id = 3; }
step s1k	{ UPDATE cc SET pid = 9 WHERE id = 3; }
step s1o	{ UPDATE cc SET pid = 1 WHERE id = 3; }
step s1d	{ DELETE FROM cc WHERE id = 3; }
step s1w	{ UPDATE cp SET w = 100 WHERE id = 4; }
step s1g	{ UPDATE cp SET grp = 0 WHERE id = 4; }
step s1a	{ UPDATE cc SET val = 0 WHERE id = 2; }
step s1e	{ UPDATE cc SET val = 50 WHERE id = 2; }
step s1p	{ UPDATE cc SET pid = 9 WHERE id = 2; }
step s1f	{ UPDATE ch SET v = -v WHERE id = 1500; }
step s1m	{ UPDATE ch SET id = 9999 WHERE id = 1500; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2b	{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2x	{ EXPLAIN (COSTS OFF) WITH u AS (UPDATE cp SET w = w + 1 WHERE grp = 4 RETURNING id) UPDATE cc SET val = val + 1 WHERE pid IN (SELECT id FROM u); }
step s2u	{ WITH u AS (UPDATE cp SET w = w + 1 WHERE grp = 4 RETURNING id) UPDATE cc SET val = val + 1 WHERE pid IN (SELECT id FROM u); }
step s2y	{ EXPLAIN (COSTS OFF) WITH u AS (UPDATE cp SET w = w + 1 WHERE grp = 4 RETURNING id, w) UPDATE cc SET val = u.w * 10000 + cc.val FROM u WHERE cc.pid = u.id; }
step s2j	{ WITH u AS (UPDATE cp SET w = w + 1 WHERE grp = 4 RETURNING id, w) UPDATE cc SET val = u.w * 10000 + cc.val FROM u WHERE cc.pid = u.id; }
step s2d	{ WITH u AS (UPDATE cp SET w = w + 1 WHERE grp = 4 RETURNING id) DELETE FROM cc WHERE pid IN (SELECT id FROM u); }
step s2i	{ WITH wc AS (INSERT INTO cl VALUES (4, 'new') RETURNING id AS newid) UPDATE cc SET val = val + 1 FROM wc WHERE pid = newid; }
step s2v	{ EXPLAIN (COSTS OFF) WITH u AS (UPDATE cq SET w = w + 1 RETURNING id, w) UPDATE cc SET val = val + u.w * 1000 FROM u WHERE cc.pid = u.id AND cc.val < 3; }
step s2h	{ WITH u AS (UPDATE cq SET w = w + 1 RETURNING id, w) UPDATE cc SET val = val + u.w * 1000 FROM u WHERE cc.pid = u.id AND cc.val < 3; }
step s2e	{ WITH u AS (UPDATE cq SET w = w + 1 RETURNING id, w) DELETE FROM cc USING u WHERE cc.pid = u.id AND cc.val < 3; }
step s2t	{ SELECT id, pid, val FROM cc WHERE id <= 3 ORDER BY id; SELECT sum(w) FROM cq; }
step s2g	{ EXPLAIN (COSTS OFF) WITH wc AS (INSERT INTO cl VALUES (1500, 'f') RETURNING id AS newid) DELETE FROM ch USING wc WHERE ch.id = newid; }
step s2f	{ WITH wc AS (INSERT INTO cl VALUES (1500, 'f') RETURNING id AS newid) DELETE FROM ch USING wc WHERE ch.id = newid; }
step s2r	{ WITH u AS (UPDATE cp SET w = w + 1 WHERE grp = 4 RETURNING id, w) UPDATE cc SET val = u.w * 10000 + cc.val FROM u WHERE cc.pid = u.id AND cc.id IN (3, 8) RETURNING cc.id, cc.val, u.id, u.w; }
step s2o	{ SELECT tableoid::regclass, id, v FROM ch WHERE id IN (1500, 9999) OR v < 0 ORDER BY id; SELECT count(*) FROM ch; SELECT * FROM cl ORDER BY id; }
step s2s	{ SELECT id, pid, val FROM cc WHERE id IN (3, 8, 53, 403) OR pid IN (4, 9) ORDER BY id; }
step s2w	{ SELECT sum(w), count(*) FILTER (WHERE w > 0) FROM cp; SELECT count(*), sum(val) FROM cc; SELECT * FROM cl ORDER BY id; }
step s2c	{ COMMIT; }

# the semi join (pid IN the CTE's ids): kept in the join, the key moved to another CTE row's, out of the join, deleted
permutation s2b s2x s1b s1v s2u s1c s2s s2w s2c
permutation s2b s1b s1k s2u s1c s2s s2w s2c
permutation s2b s1b s1o s2u s1c s2s s2w s2c
permutation s2b s1b s1d s2u s1c s2s s2w s2c
# the CTE's own row updated concurrently: kept in its quals (its write rechecks it), moved out of them
permutation s2b s1b s1w s2u s1c s2s s2w s2c
permutation s2b s1b s1g s2u s1c s2s s2w s2c
# the inner join reading a CTE column (the stored row's): kept, the key moved
permutation s2b s2y s1b s1v s2j s1c s2s s2w s2c
permutation s2b s1b s1k s2j s1c s2s s2w s2c
# the target on the hash build side, the CTE scan probing it (the rechecking pipeline enters the stored CTE row and the
# target's build reruns at its newest version): kept in the quals, moved out of them, the key moved; a DELETE
permutation s2b s2v s1b s1a s2h s1c s2t s2c
permutation s2b s1b s1e s2h s1c s2t s2c
permutation s2b s1b s1p s2h s1c s2t s2c
permutation s2b s1b s1a s2e s1c s2t s2c
permutation s2b s1b s1e s2e s1c s2t s2c
# an inheritance family's DELETE joined to an INSERT CTE (the with#287 form: each arm rechecks over the stored CTE
# row): the child row updated in place, its key moved
permutation s2b s2g s1b s1f s2f s1c s2o s2c
permutation s2b s1b s1m s2f s1c s2o s2c
# RETURNING the CTE row's columns (the stored row's, after a recheck)
permutation s2b s1b s1v s2r s1c s2s s2c
permutation s2b s1b s1k s2r s1c s2s s2c
# a DELETE beside the writing CTE: kept, moved out
permutation s2b s1b s1v s2d s1c s2s s2w s2c
permutation s2b s1b s1o s2d s1c s2s s2w s2c
# an INSERT CTE (its row joined by its RETURNING): kept, moved out
permutation s2b s1b s1v s2i s1c s2s s2w s2c
permutation s2b s1b s1o s2i s1c s2s s2w s2c

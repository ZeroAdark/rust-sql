# lockrows L1 over a hash join (main 2026-10-03 ~21:40Z item 3): the locked relation is the probe side's own scan and
# the hashed relations are REFERENCE-marked (FOR UPDATE OF a), so the recheck reruns their builds over the rows the join
# read (EvalPlanQualFetchRowMark refetches a REFERENCE row at its junk ctid) and the probe over a's newest version.
# PostgreSQL's EvalPlanQual runs a Hash only when its join asks: an outer-first join builds at its outer's first test
# row, so when a's newest version fails its quals the build never runs. Shapes: a concurrent update keeping a's quals,
# one failing them, one moving the hashed row, a hashed aggregate subquery (a COPY row), two hashed relations, a STABLE
# function reading a table another session changes, an expression that raises on a's newest version, and a VOLATILE
# call (RAISE NOTICE) in the hashed side's quals at a first recheck whose a fails (no build) and at a second recheck
# after one that passed (the join builds first). Expected output is the C oracle's (PostgreSQL 18, pgdg).
setup
{
  CREATE TABLE la (id int PRIMARY KEY, k int NOT NULL, v int NOT NULL);
  INSERT INTO la SELECT i, i % 50, i FROM generate_series(1, 400) i;
  CREATE TABLE lb (id int PRIMARY KEY, k int NOT NULL, w int NOT NULL);
  INSERT INTO lb SELECT i, i, i * 10 FROM generate_series(0, 49) i;
  CREATE TABLE lc (id int PRIMARY KEY, k int NOT NULL, z int NOT NULL);
  INSERT INTO lc SELECT i, i, i FROM generate_series(0, 49) i;
  CREATE TABLE lcfg (n int NOT NULL);
  INSERT INTO lcfg VALUES (5);
  CREATE FUNCTION lget() RETURNS int LANGUAGE sql STABLE AS 'SELECT n FROM lcfg';
  CREATE FUNCTION lnote(x int) RETURNS boolean LANGUAGE plpgsql VOLATILE AS $f$ BEGIN RAISE NOTICE 'lnote %', x; RETURN true; END $f$;
  ANALYZE la, lb, lc, lcfg;
}

teardown
{
  DROP TABLE la, lb, lc, lcfg;
  DROP FUNCTION lget(), lnote(int);
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1a	{ UPDATE la SET v = v + 1 WHERE id = 3; }
step s1af	{ UPDATE la SET v = 500 WHERE id = 3; }
step s1ar	{ UPDATE la SET v = 60 WHERE id = 3; }
step s1ak	{ UPDATE la SET k = 4 WHERE id = 3; }
step s1a53	{ UPDATE la SET v = v + 1 WHERE id = 53; }
step s1af53	{ UPDATE la SET v = 500 WHERE id = 53; }
step s1b	{ UPDATE lb SET w = w + 7 WHERE k = 3; }
step s1bk	{ UPDATE lb SET k = 9 WHERE k = 3; }
step s1bi	{ INSERT INTO lb VALUES (100, 3, 30); }
step s1g	{ UPDATE lcfg SET n = 1000; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2b	{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT a.id, a.v, b.w FROM la a JOIN lb b ON b.k = a.k WHERE a.v < 100 AND b.w IN (30, 40) FOR UPDATE OF a; }
step s2j	{ SELECT a.id, a.v, b.w FROM la a JOIN lb b ON b.k = a.k WHERE a.v < 100 AND b.w IN (30, 40) FOR UPDATE OF a; }
step s2r	{ SELECT a.id, a.v, b.w FROM la a JOIN lb b ON b.k = a.k WHERE a.v < 100 AND 1000 / (a.v - 60) > -100000 AND b.w IN (30, 40) FOR UPDATE OF a; }
step s2xs	{ EXPLAIN (COSTS OFF) SELECT a.id, s.n FROM la a JOIN (SELECT k, count(*) AS n FROM lb WHERE w IN (30, 40) GROUP BY k) s ON s.k = a.k WHERE a.v < 100 FOR UPDATE OF a; }
step s2s	{ SELECT a.id, s.n FROM la a JOIN (SELECT k, count(*) AS n FROM lb WHERE w IN (30, 40) GROUP BY k) s ON s.k = a.k WHERE a.v < 100 FOR UPDATE OF a; }
step s2x3	{ EXPLAIN (COSTS OFF) SELECT a.id, b.w, c.z FROM la a JOIN lb b ON b.k = a.k JOIN lc c ON c.k = b.k WHERE a.v < 100 AND b.w IN (30, 40) AND c.z IN (3, 4) FOR UPDATE OF a; }
step s2j3	{ SELECT a.id, b.w, c.z FROM la a JOIN lb b ON b.k = a.k JOIN lc c ON c.k = b.k WHERE a.v < 100 AND b.w IN (30, 40) AND c.z IN (3, 4) FOR UPDATE OF a; }
step s2g	{ SELECT a.id, a.v, b.w FROM la a JOIN lb b ON b.k = a.k WHERE a.v < 100 AND b.w IN (30, 40) AND b.w > lget() FOR UPDATE OF a; }
step s2xn	{ EXPLAIN (COSTS OFF) SELECT a.id, a.v, b.w FROM la a JOIN lb b ON b.k = a.k WHERE a.v < 100 AND b.id IN (3, 4) AND lnote(b.w) FOR UPDATE OF a; }
step s2n	{ SELECT a.id, a.v, b.w FROM la a JOIN lb b ON b.k = a.k WHERE a.v < 100 AND b.id IN (3, 4) AND lnote(b.w) FOR UPDATE OF a; }
step s2c	{ COMMIT; }

# a's newest version keeps its quals: the build reruns over b's row as read
permutation s2b s2x s1a s2j s1c s2c
# the hashed row updated too: b as the join read it, a's newest version
permutation s2b s1b s1a s2j s1c s2c
# the hashed row moved off the key: refetched as read, still joined
permutation s2b s1bk s1a s2j s1c s2c
# a's newest version fails its quals, or leaves the key: no row
permutation s2b s1af s2j s1c s2c
permutation s2b s1ak s2j s1c s2c
# a hashed aggregate subquery (its COPY row as read) after a concurrent insert into it
permutation s2b s2xs s1bi s1a s2s s1c s2c
# two hashed relations
permutation s2b s2x3 s1b s1a s2j3 s1c s2c
# a STABLE function reading a table the other session changed: the statement's snapshot at the recheck too
permutation s2b s1g s1a s2g s1c s2c
# an expression that raises on a's newest version
permutation s2b s1ar s2r s1c s2c
# a VOLATILE call in the hashed side's quals: no concurrency; a's newest version passing; failing at the first
# recheck (the join asks its outer first: no build); passing then failing (the second recheck builds first); and
# failing then passing
permutation s2b s2xn s2n s2c s1a s1c
permutation s2b s1a s2n s1c s2c
permutation s2b s1af s2n s1c s2c
permutation s2b s1a s1af53 s2n s1c s2c
permutation s2b s1af s1a53 s2n s1c s2c

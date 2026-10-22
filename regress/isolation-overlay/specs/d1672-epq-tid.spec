# D1672 (M8-MEMO-CTE, fail closed; found by V4R-512's review round 2): EvalPlanQual over a TID scan's row lock.
# PostgreSQL drops a TID scan's TID quals from its qual (create_tidscan_plan, create_tidrangescan_plan) and its recheck
# tests the newer version's ctid against the TID list or range (nodeTidscan.c TidRecheck, nodeTidrangescan.c
# TidRangeRecheck). s1 updates row 1, whose newer version moves to a new ctid, so s2's recheck fails and LockRows skips
# the row (its lock kept). v4's recheck in the IR (a LIMIT above the lock, V4R-316's L0-h form; an Agg or a CTE's store
# over the lock, V4R-512) reran only the ops below the lock and passed that version on; it now refuses by name (arm b:
# regress/known-residual-v4/M4/isolation/d1672-epq-tid) and the row executor answers (arm a). The runner's recheck
# (s2plain, s2prng: no LIMIT, the client as the end) is the host's TidRecheck / TidRangeRecheck and stays served.
# Expected output is the C oracle's (PostgreSQL 18, pgdg).

setup
{
  CREATE TABLE ztid (id int PRIMARY KEY, v int);
  INSERT INTO ztid SELECT i, i FROM generate_series(1, 10) i;
}

teardown
{
  DROP TABLE ztid;
}

session s1
step s1b	{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1u	{ UPDATE ztid SET v = v + 100 WHERE id = 1; }
step s1c	{ COMMIT; }

session s2
step s2b	{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2xr	{ EXPLAIN (COSTS OFF) SELECT count(*), sum(v) FROM (SELECT v FROM ztid WHERE ctid < '(0,5)' FOR UPDATE) s; }
step s2agg	{ SELECT count(*), sum(v) FROM (SELECT v FROM ztid WHERE ctid = '(0,1)' FOR UPDATE) s; }
step s2lim	{ SELECT id, v FROM ztid WHERE ctid = '(0,1)' FOR UPDATE LIMIT 1; }
step s2any	{ SELECT id, v FROM ztid WHERE ctid = ANY (ARRAY['(0,1)', '(0,2)']::tid[]) FOR UPDATE LIMIT 5; }
step s2rng	{ SELECT count(*), sum(v) FROM (SELECT v FROM ztid WHERE ctid < '(0,5)' FOR UPDATE) s; }
step s2cte	{ WITH c AS MATERIALIZED (SELECT id, v FROM ztid WHERE ctid < '(0,5)' FOR UPDATE) SELECT count(*), sum(v) FROM c; }
step s2plain	{ SELECT id, v FROM ztid WHERE ctid = '(0,1)' FOR UPDATE; }
step s2prng	{ SELECT id, v FROM ztid WHERE ctid < '(0,5)' FOR UPDATE; }
step s2c	{ COMMIT; }

# an Agg over the lock (V4R-512's product form): PG counts 0, the newer version fails the TID list
permutation s1b s1u s2b s2agg s1c s2c
# a LIMIT above the lock (L0-h): PG returns no row
permutation s1b s1u s2b s2lim s1c s2c
# a TID list under a LIMIT: row 2 only
permutation s1b s1u s2b s2any s1c s2c
# a TID range under an Agg: rows 2, 3 and 4 (count 3, sum 9); row 1's newer version is past the range
permutation s1b s1u s2b s2xr s2rng s1c s2c
# a locking CTE over a TID range (its store is the product): count 3, sum 9
permutation s1b s1u s2b s2cte s1c s2c
# does not apply: the runner's recheck (the host's TidRecheck / TidRangeRecheck), served
permutation s1b s1u s2b s2plain s1c s2c
permutation s1b s1u s2b s2prng s1c s2c

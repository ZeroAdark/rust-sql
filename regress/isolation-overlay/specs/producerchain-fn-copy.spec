# producerchain cluster A: an UPDATE / DELETE whose COPY-marked FROM row is a function's (an inlined SQL set-returning
# function's subquery, a plpgsql FunctionScan, an unnest of a composite array) and whose target sits on the hash build side
# or in the Materialize a nested loop reads, meets a target row s1 changed. EvalPlanQual reruns the plan with the target's
# newest version and the function row from its stored copy (EvalPlanQualFetchRowMark's copy leg): the function is never
# called again (ccalls counts cp's calls), s1's change to the function's source table is not seen, a version whose key
# moved off the joined row is skipped, one that still joins is written over its new values, and a deleted one is skipped.
# The same entry serves a COPY-marked OFFSET 0 subquery's row and a MERGE's (s2q, s2m: m4_d882_build_side's D652 forms).

setup
{
 CREATE TABLE ct (id int PRIMARY KEY, k int NOT NULL, x int NOT NULL);
 INSERT INTO ct SELECT i, i, 0 FROM generate_series(1, 900) i;
 CREATE TABLE cs (k int, v text, w int);
 INSERT INTO cs SELECT i, 'v' || i, i % 4 FROM generate_series(1, 3000) i;
 CREATE TYPE cty AS (k int, v text, w int);
 CREATE FUNCTION ci() RETURNS SETOF cty LANGUAGE sql STABLE AS $$ SELECT * FROM cs OFFSET 0 $$;
 CREATE SEQUENCE ccalls;
 CREATE FUNCTION cp() RETURNS SETOF cty LANGUAGE plpgsql VOLATILE ROWS 3000 AS $$ BEGIN PERFORM nextval('ccalls'); RETURN QUERY SELECT * FROM cs; END $$;
 CREATE TABLE cn (id int PRIMARY KEY, active bool NOT NULL, x int NOT NULL);
 INSERT INTO cn VALUES (1, false, 1), (10, false, 10), (11, true, 11);
 CREATE TYPE cn_in AS (id int, x int);
 CREATE TYPE cn_key AS (id int);
 CREATE FUNCTION cn_kt(cn) RETURNS cn_key LANGUAGE sql AS $$ SELECT $1.id $$;
 CREATE FUNCTION cn_ki(cn_in) RETURNS cn_key LANGUAGE sql AS $$ SELECT $1.id $$;
 ANALYZE ct, cs, cn;
}

teardown
{
 DROP TABLE ct;
 DROP FUNCTION ci();
 DROP FUNCTION cp();
 DROP TABLE cs;
 DROP TYPE cty;
 DROP SEQUENCE ccalls;
 DROP FUNCTION cn_kt(cn);
 DROP FUNCTION cn_ki(cn_in);
 DROP TABLE cn;
 DROP TYPE cn_in;
 DROP TYPE cn_key;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1x	{ UPDATE ct SET x = x + 1000 WHERE id IN (300, 600, 900); }
step s1k	{ UPDATE ct SET k = k + 5000 WHERE id = 450; }
step s1d	{ DELETE FROM ct WHERE id = 750; }
step s1s	{ UPDATE cs SET v = 'changed' WHERE k IN (300, 600); }
step s1n	{ UPDATE cn SET x = x + 100 WHERE id = 10; }
step s1m	{ UPDATE cn SET id = 12 WHERE id = 11; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_mergejoin = off; SET enable_memoize = off; SET max_parallel_workers_per_gather = 0; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2u	{ UPDATE ct SET x = ct.x + 1 FROM ci() s WHERE s.k = ct.k AND ct.id % 150 = 0 AND s.w <> 3 RETURNING ct.id, ct.k, ct.x, s.v, s; }
step s2p	{ UPDATE ct SET x = ct.x + 2 FROM cp() s WHERE s.k = ct.k AND ct.id % 150 = 0 AND s.w <> 1 RETURNING ct.id, ct.x, s.v; }
step s2d	{ DELETE FROM ct USING ci() s WHERE s.k = ct.k AND ct.id % 150 = 0 AND s.w IN (0, 2) RETURNING ct.id, s.v; }
step s2n	{ UPDATE cn SET active = true, x = cn.x + i.x FROM unnest(ARRAY[(10, 5), (11, 7)]::cn_in[]) i WHERE cn_kt(cn.*) = cn_ki(i.*) RETURNING cn.id, cn.x, i; }
step s2q	{ UPDATE ct SET x = ct.x + 3 FROM (SELECT * FROM cs OFFSET 0) s WHERE s.k = ct.k AND ct.id % 150 = 0 AND s.w <> 3 RETURNING ct.id, ct.k, ct.x, s.v; }
step s2m	{ MERGE INTO ct USING (SELECT * FROM cs OFFSET 0) s ON s.k = ct.k AND ct.id % 150 = 0 WHEN MATCHED AND s.w <> 1 THEN UPDATE SET x = ct.x + 4; }
step s2c	{ COMMIT; }

session s3
setup		{ SET enable_mergejoin = off; SET enable_memoize = off; SET max_parallel_workers_per_gather = 0; }
step s3sel	{ SELECT count(*), sum(k), sum(x) FROM ct; }
step s3row	{ SELECT id, k, x FROM ct WHERE id IN (150, 300, 450, 600, 750, 900) ORDER BY id; }
step s3cn	{ SELECT id, active, x FROM cn ORDER BY id; }
step s3calls	{ SELECT last_value FROM ccalls; }
step s3pu	{ EXPLAIN (COSTS OFF) UPDATE ct SET x = ct.x + 1 FROM ci() s WHERE s.k = ct.k AND ct.id % 150 = 0 AND s.w <> 3; }
step s3pp	{ EXPLAIN (COSTS OFF) UPDATE ct SET x = ct.x + 2 FROM cp() s WHERE s.k = ct.k AND ct.id % 150 = 0 AND s.w <> 1; }
step s3pd	{ EXPLAIN (COSTS OFF) DELETE FROM ct USING ci() s WHERE s.k = ct.k AND ct.id % 150 = 0 AND s.w IN (0, 2); }
step s3pn	{ EXPLAIN (COSTS OFF) UPDATE cn SET active = true, x = cn.x + i.x FROM unnest(ARRAY[(10, 5), (11, 7)]::cn_in[]) i WHERE cn_kt(cn.*) = cn_ki(i.*); }
step s3pq	{ EXPLAIN (COSTS OFF) UPDATE ct SET x = ct.x + 3 FROM (SELECT * FROM cs OFFSET 0) s WHERE s.k = ct.k AND ct.id % 150 = 0 AND s.w <> 3; }
step s3pm	{ EXPLAIN (COSTS OFF) MERGE INTO ct USING (SELECT * FROM cs OFFSET 0) s ON s.k = ct.k AND ct.id % 150 = 0 WHEN MATCHED AND s.w <> 1 THEN UPDATE SET x = ct.x + 4; }

permutation s3pu s3pp s3pd s3pn s1c s2c
permutation s1x s1k s1d s1s s2u s1c s2c s3sel s3row
permutation s1x s1k s1d s1s s2p s1c s2c s3sel s3row s3calls
permutation s1x s1k s1d s2d s1c s2c s3sel s3row
permutation s1n s1m s2n s1c s2c s3cn
permutation s2u s1x s2c s1c s3sel s3row
permutation s3pq s3pm s1c s2c
permutation s1x s1k s1d s1s s2q s1c s2c s3sel s3row
permutation s1x s1k s1d s1s s2m s1c s2c s3sel s3row

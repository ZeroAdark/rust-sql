# D3379 ROWC-JOINCOPY: a data-modifying CTE's DELETE / UPDATE whose target's scan probes the hash of a join of a plain
# relation (REFERENCE-marked) and another data-modifying CTE's rows (COPY-marked), while another session changes a target
# row (READ COMMITTED). EvalPlanQual refetches the plain relation's row at its junk ctid and hands the CTE Scan its stored
# row, then tests the target's newest version against that joined row: an in-place change that still joins is written
# (the UPDATE over the new values), a change that fails the target's own qual or the join qual is skipped, a deleted row
# is skipped. Expected output is PostgreSQL 18.6's (the C oracle).

setup
{
 CREATE TABLE rj_base (id int PRIMARY KEY, grp int NOT NULL, amt int NOT NULL);
 CREATE TABLE rj_side (id int PRIMARY KEY, v int NOT NULL, tag text NOT NULL);
 INSERT INTO rj_base SELECT g, g % 400, g * 10 FROM generate_series(1, 2000) g;
 INSERT INTO rj_side SELECT g, g % 37, CASE WHEN g % 2 = 0 THEN 'even' ELSE 'odd' END FROM generate_series(1, 3000) g;
 ANALYZE rj_base, rj_side;
}

teardown
{
 DROP TABLE rj_base, rj_side;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE rj_side SET v = v + 10000 WHERE id IN (1, 401); }
step s1tag	{ UPDATE rj_side SET tag = 'even' WHERE id = 801; }
step s1del	{ DELETE FROM rj_side WHERE id = 401; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; SET max_parallel_workers_per_gather = 0; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2d	{ WITH u AS (UPDATE rj_base SET amt = amt + 1 WHERE grp = 1 RETURNING id), d AS (DELETE FROM rj_side t USING (SELECT b.id FROM rj_base b JOIN u ON u.id = b.id WHERE b.amt > 5) x WHERE x.id = t.id RETURNING t.id, t.v) SELECT id, v FROM d ORDER BY id; }
step s2u	{ WITH u AS (UPDATE rj_base SET amt = amt + 1 WHERE grp = 1 RETURNING id, amt), w AS (UPDATE rj_side t SET v = t.v + x.amt FROM (SELECT b.id, u.amt FROM rj_base b JOIN u ON u.id = b.id) x WHERE x.id = t.id AND t.tag = 'odd' AND x.amt > t.v RETURNING t.id, t.v) SELECT id, v FROM w ORDER BY id; }
step s2exu	{ EXPLAIN (COSTS OFF) WITH u AS (UPDATE rj_base SET amt = amt + 1 WHERE grp = 1 RETURNING id, amt), w AS (UPDATE rj_side t SET v = t.v + x.amt FROM (SELECT b.id, u.amt FROM rj_base b JOIN u ON u.id = b.id) x WHERE x.id = t.id AND t.tag = 'odd' AND x.amt > t.v RETURNING t.id, t.v) SELECT id, v FROM w ORDER BY id; }
step s2exp	{ EXPLAIN (COSTS OFF) WITH u AS (UPDATE rj_base SET amt = amt + 1 WHERE grp = 1 RETURNING id), d AS (DELETE FROM rj_side t USING (SELECT b.id FROM rj_base b JOIN u ON u.id = b.id WHERE b.amt > 5) x WHERE x.id = t.id RETURNING t.id, t.v) SELECT id, v FROM d ORDER BY id; }
step s2sel	{ SELECT id, v, tag FROM rj_side WHERE id % 400 = 1 ORDER BY id; }
step s2c	{ COMMIT; }

permutation s2exp s1v s2d s1c s2sel s2c
permutation s1del s2d s1c s2sel s2c
permutation s1v s2u s1c s2sel s2c
permutation s2exu s1tag s2u s1c s2sel s2c
permutation s1del s2u s1c s2sel s2c

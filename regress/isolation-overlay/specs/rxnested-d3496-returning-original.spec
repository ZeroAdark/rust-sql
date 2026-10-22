# D3496: UPDATE / DELETE .. FROM a flattened UNION ALL subquery whose RETURNING reads the subquery's columns, while another
# session re-keys the matched row (READ COMMITTED; nested loops and merge joins off). The subquery's COPY mark names a
# relation no scan carries, so PostgreSQL 18.6's EvalPlanQual scans the union's members again and joins them with the
# row's newest version: the UPDATE takes its values from that re-joined row, while RETURNING projects over the ORIGINAL
# plan row (ExecProcessReturning's planSlot), so the subquery's columns are the ones the statement first joined.
#  (a) the settled form: the result relation hashed, the union (a scanned relation under a qual, a constant row) probing
#  (b) the same with a bare relation member, and the DELETE
#  (c) the members hashed, the result relation probing (bare relation members), UPDATE and DELETE
# The EXPLAIN steps pin the plans. Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE ur_t (id int PRIMARY KEY, v int NOT NULL, hits int NOT NULL DEFAULT 0);
 INSERT INTO ur_t SELECT i, i * 10, 0 FROM generate_series(1, 20) i;
 CREATE TABLE ur_s (k int, w int);
 INSERT INTO ur_s SELECT 100 + i, i FROM generate_series(1, 3000) i;
 INSERT INTO ur_s VALUES (5, 50), (25, 250);
 CREATE TABLE ua_t (id int PRIMARY KEY, hits int NOT NULL DEFAULT 0);
 INSERT INTO ua_t SELECT i, 0 FROM generate_series(1, 2000) i WHERE i % 5 <> 0;
 CREATE TABLE ua_a (k int, w int);
 INSERT INTO ua_a VALUES (3, 1), (6, 2);
 CREATE TABLE ua_b (k int, w int);
 INSERT INTO ua_b VALUES (4, 101), (10, 103);
 ANALYZE ur_t, ur_s, ua_t, ua_a, ua_b;
}

teardown
{
 DROP TABLE ur_t, ur_s, ua_t, ua_a, ua_b;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1key	{ UPDATE ur_t SET id = 25 WHERE id = 5; }
step s1akey	{ UPDATE ua_t SET id = 10 WHERE id = 3; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2exp	{ EXPLAIN (COSTS OFF) UPDATE ur_t t SET hits = t.hits + s.w FROM (SELECT k, w FROM ur_s WHERE w > 0 UNION ALL SELECT 7, 1) s WHERE t.id = s.k RETURNING t.id, t.hits, s.k, s.w; }
step s2u	{ UPDATE ur_t t SET hits = t.hits + s.w FROM (SELECT k, w FROM ur_s WHERE w > 0 UNION ALL SELECT 7, 1) s WHERE t.id = s.k RETURNING t.id, t.hits, s.k, s.w; }
step s2ub	{ UPDATE ur_t t SET hits = t.hits + s.w FROM (SELECT k, w FROM ur_s UNION ALL SELECT 7, 1) s WHERE t.id = s.k RETURNING t.id, s.w * 2 + t.hits AS x; }
step s2d	{ DELETE FROM ur_t t USING (SELECT k, w FROM ur_s WHERE w > 0 UNION ALL SELECT 7, 1) s WHERE t.id = s.k RETURNING t.id, t.v, s.k, s.w; }
step s2sel	{ SELECT id, v, hits FROM ur_t WHERE id IN (5, 7, 25) ORDER BY id; }
step s2aexp	{ EXPLAIN (COSTS OFF) UPDATE ua_t t SET hits = t.hits + s.w FROM (SELECT k, w FROM ua_a UNION ALL SELECT k, w FROM ua_b) s WHERE t.id = s.k RETURNING t.id, t.hits, s.k, s.w; }
step s2au	{ UPDATE ua_t t SET hits = t.hits + s.w FROM (SELECT k, w FROM ua_a UNION ALL SELECT k, w FROM ua_b) s WHERE t.id = s.k RETURNING t.id, t.hits, s.k, s.w; }
step s2ad	{ DELETE FROM ua_t t USING (SELECT k, w FROM ua_a UNION ALL SELECT k, w FROM ua_b) s WHERE t.id = s.k RETURNING t.id, s.k, s.w; }
step s2asel	{ SELECT id, hits FROM ua_t WHERE hits <> 0 OR id <= 11 ORDER BY id; }

permutation s2exp s1key s2u s1c s2sel
permutation s1key s2ub s1c s2sel
permutation s1key s2d s1c s2sel
permutation s2aexp s1akey s2au s1c s2asel
permutation s1akey s2ad s1c s2asel

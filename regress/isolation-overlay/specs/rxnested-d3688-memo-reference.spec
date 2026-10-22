# D3688: UPDATE / DELETE planned as a Nested Loop whose outer is the target's index scan and whose inner is a Memoize over
# another relation's index scan parameterized by the target row (sequential and bitmap scans off), while another session
# changes the target row (READ COMMITTED). PostgreSQL 18.6's EvalPlanQual runs a fresh Memoize, the inner index scan
# handed the row its REFERENCE mark fetched, its index condition checked again under the newest version's parameter.
#  (a) the row updated in place, its key unchanged: written over the newest version
#  (b) the row's key moved so the fetched inner row no longer matches (another inner row would): skipped
#  (c) a DELETE over the row updated in place: deleted
#  (d) through a WITH CHECK OPTION view of the same join as EXISTS, the row updated in place: written, the check passes
# The EXPLAIN steps pin the plans. Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE mr_ref (a int PRIMARY KEY, b int);
 INSERT INTO mr_ref SELECT g, g * 10 FROM generate_series(1, 20) g;
 ANALYZE mr_ref;
 CREATE TABLE mr_t (id int PRIMARY KEY, a int, n text);
 INSERT INTO mr_t VALUES (1, 3, 'x'), (2, 18, 'y'), (3, 5, 'z');
 INSERT INTO mr_t SELECT g + 100, g, 's' FROM generate_series(0, 19) g;
 CREATE VIEW mr_v AS SELECT * FROM mr_t t WHERE EXISTS (SELECT 1 FROM mr_ref r WHERE r.a = t.a + 1) WITH CHECK OPTION;
}

teardown
{
 DROP VIEW mr_v;
 DROP TABLE mr_t, mr_ref;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1n	{ UPDATE mr_t SET n = n || '!' WHERE id = 1; }
step s1a	{ UPDATE mr_t SET a = 7 WHERE id = 1; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_seqscan = off; SET enable_bitmapscan = off; }
step s2exp	{ EXPLAIN (COSTS OFF) UPDATE mr_t t SET n = n || '+' FROM mr_ref r WHERE t.id IN (1, 3) AND r.a = t.a + 1; }
step s2u	{ UPDATE mr_t t SET n = t.n || '+' || r.b FROM mr_ref r WHERE t.id IN (1, 3) AND r.a = t.a + 1 RETURNING t.id, t.a, t.n; }
step s2d	{ DELETE FROM mr_t t USING mr_ref r WHERE t.id IN (1, 3) AND r.a = t.a + 1 RETURNING t.id, t.a, t.n; }
step s2vexp	{ EXPLAIN (COSTS OFF) UPDATE mr_v SET n = n || '+' WHERE id IN (1, 3); }
step s2vu	{ UPDATE mr_v SET n = n || '+' WHERE id IN (1, 3) RETURNING *; }
step s2sel	{ SELECT id, a, n FROM mr_t WHERE id < 100 ORDER BY id; }

permutation s2exp s1n s2u s1c s2sel
permutation s1a s2u s1c s2sel
permutation s1n s2d s1c s2sel
permutation s2vexp s1n s2vu s1c s2sel

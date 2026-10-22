# D3689: UPDATE / DELETE whose EXISTS PostgreSQL turns into an inner Hash Join probed by a HashAggregate over the other
# relation's scan (the semi join made unique), the target's scan hashed, while another session changes the target row
# (READ COMMITTED). PostgreSQL 18.6's EvalPlanQual reruns the aggregate over the other relation's row its REFERENCE mark
# fetched (the group's first row) and joins it with the newest version.
#  (a) the row updated in place, its key unchanged: written over the newest version
#  (b) the row's key moved to another group's value: no longer joins the fetched row, skipped
#  (c) a DELETE over the row updated in place: deleted
#  (d) through a WITH CHECK OPTION view of the same EXISTS, the row updated in place: written, the check passes
# The EXPLAIN steps pin the plans. Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE ga_r (t text, n int);
 INSERT INTO ga_r VALUES ('apple', 1), ('banana', 2), ('cherry', 3), ('apple', 4);
 CREATE TABLE ga_t (id int PRIMARY KEY, t text, v int);
 INSERT INTO ga_t VALUES (1, 'apple', 0), (2, 'banana', 0), (3, 'fig', 0);
 CREATE VIEW ga_v AS SELECT * FROM ga_t c WHERE EXISTS (SELECT 1 FROM ga_r r WHERE r.t = c.t) WITH CHECK OPTION;
}

teardown
{
 DROP VIEW ga_v;
 DROP TABLE ga_t, ga_r;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE ga_t SET v = v + 100 WHERE id = 1; }
step s1t	{ UPDATE ga_t SET t = 'banana' WHERE id = 1; }
step s1c	{ COMMIT; }

session s2
step s2exp	{ EXPLAIN (COSTS OFF) UPDATE ga_t c SET v = v + 1 WHERE id = 1 AND EXISTS (SELECT 1 FROM ga_r r WHERE r.t = c.t); }
step s2u	{ UPDATE ga_t c SET v = v + 1 WHERE id = 1 AND EXISTS (SELECT 1 FROM ga_r r WHERE r.t = c.t) RETURNING *; }
step s2d	{ DELETE FROM ga_t c WHERE id = 1 AND EXISTS (SELECT 1 FROM ga_r r WHERE r.t = c.t) RETURNING *; }
step s2vexp	{ EXPLAIN (COSTS OFF) UPDATE ga_v SET v = v + 1 WHERE id = 1; }
step s2vu	{ UPDATE ga_v SET v = v + 1 WHERE id = 1 RETURNING *; }
step s2sel	{ SELECT id, t, v FROM ga_t ORDER BY id; }

permutation s2exp s1v s2u s1c s2sel
permutation s1t s2u s1c s2sel
permutation s1v s2d s1c s2sel
permutation s2vexp s1v s2vu s1c s2sel

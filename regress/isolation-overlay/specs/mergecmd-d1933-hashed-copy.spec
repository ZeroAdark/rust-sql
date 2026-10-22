# mergecmd D1933 hashed COPY (gen mergecmd source-distinct-on's form, mg-0018 / 0046 / 0074 / 0102 / 0130, when the
# target's statistics are stale): a MERGE whose source is a DISTINCT ON subquery (its row a COPY mark) and whose target
# the planner hashes on the build side, while another session changes a matched row (READ COMMITTED). PostgreSQL 18.6's
# ExecMergeMatched rechecks the newest version under EvalPlanQual with the source row handed over whole (the COPY mark:
# the subquery, its Sort and its Unique never run again) and the target's scan returning that version alone. The
# tables are never analyzed, so s2exp pins the plan (Hash Join, the target under the Hash).
#  (a) s1 updates the matched row: the UPDATE reads the newest version, its SET a column of the stored source row
#  (b) s1 moves the row's key away: the recheck finds no match, the row is left
#  (c) s1 deletes the row: no action (no NOT MATCHED action)
#  (d) s1 makes the WHEN qual false on the newest version: no action
#  (e) a DELETE action over the newest version
#  (f) s1 aborts: the original version is updated (control)
# Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE hc_t (id int PRIMARY KEY, n int NOT NULL, grp int, val int NOT NULL, flag boolean, hits int NOT NULL DEFAULT 0);
 INSERT INTO hc_t SELECT i, i, i % 13, (i * 37) % 1000, false, 0 FROM generate_series(1, 2000) i;
 CREATE TABLE hc_s (id int, n int, grp int, val int);
 INSERT INTO hc_s SELECT i, i, i % 13, (i * 53) % 1000 FROM generate_series(1801, 2200) i;
}

teardown
{
 DROP TABLE hc_t, hc_s;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1upd	{ UPDATE hc_t SET hits = hits + 10, val = val + 1 WHERE id = 1849; }
step s1key	{ UPDATE hc_t SET id = 1849000 WHERE id = 1849; }
step s1del	{ DELETE FROM hc_t WHERE id = 1849; }
step s1qual	{ UPDATE hc_t SET hits = 100 WHERE id = 1849; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
step s2exp	{ EXPLAIN (COSTS OFF) MERGE INTO hc_t t USING (SELECT DISTINCT ON (grp) id, val FROM hc_s ORDER BY grp, val DESC, n) s ON t.id = s.id WHEN MATCHED AND t.hits < 50 THEN UPDATE SET flag = true, hits = t.hits + 1, val = s.val; }
step s2upd	{ MERGE INTO hc_t t USING (SELECT DISTINCT ON (grp) id, val FROM hc_s ORDER BY grp, val DESC, n) s ON t.id = s.id WHEN MATCHED AND t.hits < 50 THEN UPDATE SET flag = true, hits = t.hits + 1, val = s.val; }
step s2del	{ MERGE INTO hc_t t USING (SELECT DISTINCT ON (grp) id, val FROM hc_s ORDER BY grp, val DESC, n) s ON t.id = s.id WHEN MATCHED AND t.hits >= 10 THEN DELETE WHEN MATCHED THEN UPDATE SET flag = true; }
step s2sel	{ SELECT id, n, val, flag, hits FROM hc_t WHERE id IN (1811, 1830, 1849, 1943, 1962, 1981, 1849000) ORDER BY id; }

permutation s2exp s1upd s2upd s1c s2sel
permutation s1key s2upd s1c s2sel
permutation s1del s2upd s1c s2sel
permutation s1qual s2upd s1c s2sel
permutation s1upd s2del s1c s2sel
permutation s1upd s2upd s1a s2sel

# writesubplan (D652's rest): a write over another pipeline's product (a hash join: UPDATE ... FROM, DELETE ... USING,
# MERGE) whose RETURNING list reads an aggregate SubPlan, a rerun set the recheck steps past, while another session
# changes the matched rows (READ COMMITTED). PostgreSQL 18.6 evaluates the list per returned row after the write, over
# the rechecked row: the recheck node reruns the set at the step's read under the producers' frame (exec admit/entry
# run.rs, as the runner's own recheck), its arguments the newest version's.
#  (a) MERGE: a matched row concurrently deleted falls through to the NOT MATCHED INSERT; one concurrently updated
#      is rechecked (OLD = s1's version); a one-row source's MERGE (a nested loop) over the rechecked row
#  (b) UPDATE ... FROM and DELETE ... USING over a row s1 updated (rechecked) or deleted (skipped)
# Expected output is PostgreSQL 18.6's (the C oracle).

setup
{
 CREATE TABLE wra (k int PRIMARY KEY, v int, note text);
 CREATE TABLE wrs (k int, v int);
 CREATE TABLE wrx (action text, abbrev text);
 INSERT INTO wra VALUES (4, 4, 'i'), (5, 5, 'i'), (6, 6, 'i');
 INSERT INTO wrs SELECT g, g * 10 FROM generate_series(4, 50) g;
 INSERT INTO wrx VALUES ('INSERT', 'ins'), ('UPDATE', 'upd'), ('DELETE', 'del');
 ANALYZE wra, wrs, wrx;
}

teardown
{
 DROP TABLE wra, wrs, wrx;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1del4	{ DELETE FROM wra WHERE k = 4; }
step s1upd5	{ UPDATE wra SET v = 500 WHERE k = 5; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2hash	{ MERGE INTO wra t USING wrs s ON t.k = s.k WHEN MATCHED AND t.k IN (4, 5) THEN UPDATE SET v = t.v + s.v, note = 'u' WHEN NOT MATCHED AND s.k = 4 THEN INSERT VALUES (s.k, s.v + 1, 'nm') RETURNING (SELECT abbrev FROM wrx WHERE action = merge_action()) AS act, old.v AS old_v, (SELECT count(*) FROM wrs x WHERE x.k < t.k + 3) AS n, (SELECT count(*) FROM wrs x WHERE x.v <= t.v) AS m, t.*; }
step s2upd	{ UPDATE wra t SET v = t.v + s.v FROM wrs s WHERE t.k = s.k AND t.k IN (4, 5) RETURNING t.k, (SELECT count(*) FROM wrs x WHERE x.v <= t.v) AS m, (SELECT count(*) FROM wrs x WHERE x.v <= old.v) AS old_m, t.v; }
step s2del	{ DELETE FROM wra t USING wrs s WHERE t.k = s.k AND t.k IN (4, 5) RETURNING t.k, (SELECT count(*) FROM wrs x WHERE x.v <= t.v) AS m, t.v; }
step s2sel	{ SELECT * FROM wra ORDER BY k; }

session s3
step s3agg	{ MERGE INTO wra t USING (SELECT 5 AS k, 50 AS v) s ON t.k = s.k WHEN MATCHED THEN UPDATE SET v = t.v + s.v, note = 'a' RETURNING (SELECT count(*) FROM wrs x WHERE x.v <= t.v) AS m, (SELECT old.v) AS old_v, new.v; }

permutation s1del4 s2hash s1c s2sel
permutation s1upd5 s2hash s1c s2sel
permutation s1upd5 s3agg s1c s2sel
permutation s1upd5 s2upd s1c s2sel
permutation s1del4 s2del s1c s2sel
permutation s1upd5 s2del s1c s2sel

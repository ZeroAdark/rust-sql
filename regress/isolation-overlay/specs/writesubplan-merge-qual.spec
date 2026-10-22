# writesubplan (regress merge#340, D1112): a MERGE whose join filter reads a correlated SubPlan (t.b < (SubPlan 1), the
# count over another table PostgreSQL places on the Hash Join), while another session updates or deletes a matched
# target row. s2's MERGE waits on the row; once s1 commits, EvalPlanQual re-evaluates the join over the new version,
# the join filter's SubPlan included, and the WHEN conditions after it:
#  (a) s1 raises t.b past the count: the pair no longer joins, the row takes no action
#  (b) s1 lowers t.c below the count: the pair joins, the WHEN MATCHED AND t.c > cnt condition fails
#  (c) s1 deletes the row: nothing to update
#  (d) the SubPlan correlated with the target row too (r.ab = t.a + t.b + s.b): s1's new t.b changes the count, the pair
#      stops matching and the source row takes the WHEN NOT MATCHED INSERT
#  (e) as (d) with s1 rolled back: the old version, matched
#  (f) as (d) over a Merge Join (hash and nested-loop joins off): the recheck reruns the merge join's filter
# Expected output is PostgreSQL 18.6's (the C oracle).

setup
{
 CREATE TABLE wmq_src (a int, b int, c int, d int);
 CREATE TABLE wmq_tgt (a int, b int, c int, d int);
 CREATE TABLE wmq_ref (ab int, cd int);
 INSERT INTO wmq_src VALUES (1, 1, 5, 1), (2, 0, 3, 1);
 INSERT INTO wmq_ref VALUES (2, 4), (2, 2), (2, 2);
 INSERT INTO wmq_tgt VALUES (1, 0, 9, 0), (2, 0, 9, 0);
}

teardown
{
 DROP TABLE wmq_src, wmq_tgt, wmq_ref;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1b	{ UPDATE wmq_tgt SET b = 1 WHERE a = 1; }
step s1c2	{ UPDATE wmq_tgt SET c = 1 WHERE a = 2; }
step s1del	{ DELETE FROM wmq_tgt WHERE a = 1; }
step s1c	{ COMMIT; }
step s1r	{ ROLLBACK; }

session s2
step s2m	{
  MERGE INTO wmq_tgt t USING (SELECT *, (SELECT count(*) FROM wmq_ref r WHERE r.ab = s.a + s.b AND r.cd = s.c - s.d) cnt FROM wmq_src s) s
  ON t.a = s.a AND t.b < s.cnt WHEN MATCHED AND t.c > s.cnt THEN UPDATE SET (b, c) = (SELECT s.b, s.cnt);
}
step s2mt	{
  MERGE INTO wmq_tgt t USING wmq_src s ON t.a = s.a AND t.b < (SELECT count(*) FROM wmq_ref r WHERE r.ab = t.a + t.b + s.b)
  WHEN MATCHED THEN UPDATE SET d = t.d + 1 WHEN NOT MATCHED THEN INSERT VALUES (s.a + 10, s.b, s.c, s.d);
}
step s2mj	{ SET enable_hashjoin = off; SET enable_nestloop = off; }
step s2mjr	{ RESET enable_hashjoin; RESET enable_nestloop; }

session s3
step s3sel	{ SELECT * FROM wmq_tgt ORDER BY a; }

permutation s1b s2m s1c s3sel
permutation s1c2 s2m s1c s3sel
permutation s1del s2m s1c s3sel
permutation s1b s2mt s1c s3sel
permutation s1b s2mt s1r s3sel
permutation s2mj s1b s2mt s1c s3sel s2mjr

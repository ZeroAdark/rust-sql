# writesubplan (gen mg-0146 / mg-0159): a MERGE whose TARGET PostgreSQL hashes (Hash Right Join, the source probing), with
# WHEN MATCHED UPDATE and WHEN NOT MATCHED BY SOURCE UPDATE: the matched rows come out of the probe, the NOT MATCHED BY
# SOURCE rows are the target rows the probe never matched (drained), each carrying the target's ctid. Another session
# updates or deletes a target row while the MERGE waits on it; EvalPlanQual rechecks it over the new version:
#  (a) s1 changes a matched row's non-key column: still matched, updated over the new version
#  (b) s1 moves a matched row's key off every source row: it no longer matches
#  (c) s1 changes a NOT MATCHED BY SOURCE row's non-key column: still unmatched, its action applies over the new version
#  (d) s1 deletes a matched row: nothing to update
#  (e) s1 moves a NOT MATCHED BY SOURCE row's key onto a source row
# Expected output is PostgreSQL 18.6's (the C oracle).

setup
{
 CREATE TABLE wmh_t (id int, val int);
 CREATE TABLE wmh_s (id int);
 INSERT INTO wmh_t SELECT g, g * 10 FROM generate_series(1, 5) g;
 INSERT INTO wmh_s SELECT g FROM generate_series(3, 1000) g;
 ANALYZE wmh_t, wmh_s;
}

teardown
{
 DROP TABLE wmh_t, wmh_s;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE wmh_t SET val = 99 WHERE id = 4; }
step s1k	{ UPDATE wmh_t SET id = 2000 WHERE id = 4; }
step s1b	{ UPDATE wmh_t SET val = 77 WHERE id = 1; }
step s1del	{ DELETE FROM wmh_t WHERE id = 4; }
step s1kb	{ UPDATE wmh_t SET id = 3 WHERE id = 1; }
step s1c	{ COMMIT; }

session s2
step s2m	{
  MERGE INTO wmh_t t USING wmh_s s ON t.id = s.id
  WHEN MATCHED THEN UPDATE SET val = t.val + 1 WHEN NOT MATCHED BY SOURCE THEN UPDATE SET val = -t.val;
}

session s3
step s3sel	{ SELECT * FROM wmh_t ORDER BY id, val; }

permutation s1v s2m s1c s3sel
permutation s1k s2m s1c s3sel
permutation s1b s2m s1c s3sel
permutation s1del s2m s1c s3sel
permutation s1kb s2m s1c s3sel

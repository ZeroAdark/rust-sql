# writesubplan (regress merge#368 / #402, gen mg-0181): a MERGE through a list-partitioned root whose result relations'
# scans (an Append of three leaves, one with another column order and a dropped column) probe the hashed source (Hash Full
# Join), with WHEN MATCHED UPDATE, WHEN NOT MATCHED INSERT routed through the root and WHEN NOT MATCHED BY SOURCE UPDATE,
# while another session updates, moves or deletes a target row. s2 waits on the row; once s1 commits, EvalPlanQual
# rechecks it over the new version in its own leaf:
#  (a) s1 changes a matched row's non-key column: still matched, updated over the new version
#  (b) s1 changes a matched row's key (same leaf) off every source row: NOT MATCHED BY SOURCE now, the source row inserted
#  (c) s1 moves a matched row into another leaf: the old version is gone, the source row takes the NOT MATCHED INSERT
#  (d) s1 deletes a matched row: the source row takes the NOT MATCHED INSERT
#  (e) s1 changes a NOT MATCHED BY SOURCE row's non-key column: its action applies over the new version
#  (f) as (a) with s1 rolled back: the old version
# Expected output is PostgreSQL 18.6's (the C oracle).

setup
{
 CREATE TABLE wmp_t (tid int, bal int, val text) PARTITION BY LIST (tid);
 CREATE TABLE wmp_t1 PARTITION OF wmp_t FOR VALUES IN (1, 2, 3, 4);
 CREATE TABLE wmp_t2 (val text, x int, bal int, tid int);
 ALTER TABLE wmp_t2 DROP x;
 ALTER TABLE wmp_t ATTACH PARTITION wmp_t2 FOR VALUES IN (5, 6, 7, 8);
 CREATE TABLE wmp_t3 PARTITION OF wmp_t DEFAULT;
 CREATE TABLE wmp_s (sid int, delta int);
 INSERT INTO wmp_t SELECT g, g * 100, 'init' FROM generate_series(1, 40) g WHERE g % 2 = 1;
 INSERT INTO wmp_s SELECT g, g * 10 FROM generate_series(1, 8) g;
 ANALYZE wmp_t, wmp_s;
}

teardown
{
 DROP TABLE wmp_t, wmp_s;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE wmp_t SET bal = bal + 1 WHERE tid = 5; }
step s1k	{ UPDATE wmp_t SET tid = 2 WHERE tid = 3; }
step s1mv	{ UPDATE wmp_t SET tid = 9 WHERE tid = 7; }
step s1del	{ DELETE FROM wmp_t WHERE tid = 1; }
step s1b	{ UPDATE wmp_t SET bal = 7 WHERE tid = 11; }
step s1c	{ COMMIT; }
step s1r	{ ROLLBACK; }

session s2
step s2m	{
  MERGE INTO wmp_t t USING wmp_s s ON t.tid = s.sid
  WHEN MATCHED THEN UPDATE SET bal = t.bal + s.delta, val = t.val || ' upd'
  WHEN NOT MATCHED THEN INSERT VALUES (s.sid, s.delta, 'ins')
  WHEN NOT MATCHED BY SOURCE THEN UPDATE SET val = t.val || ' bys';
}

session s3
step s3sel	{ SELECT tableoid::regclass, * FROM wmp_t ORDER BY tid, bal, val; }

permutation s1v s2m s1c s3sel
permutation s1k s2m s1c s3sel
permutation s1mv s2m s1c s3sel
permutation s1del s2m s1c s3sel
permutation s1b s2m s1c s3sel
permutation s1v s2m s1r s3sel

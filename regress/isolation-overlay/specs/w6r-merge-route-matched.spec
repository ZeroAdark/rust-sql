# M8-WRITE W6-MERGE-ROUTE-MATCHED: a MERGE through a partitioned root whose MATCHED actions update or delete, served over
# several leaves (the write streamed into the Append's arms, hosts MultiTargetHost), under a concurrent change of the row
# it matches (READ COMMITTED). The source (4 rows) is hashed; the root's 3 leaves are the probe side. After s1 commits or
# aborts, PostgreSQL 18.6's ExecMergeMatched:
#  (a) s1 updated the row: s2 waits, then rechecks the newest version and reruns the action list from its first action
#      (the first action's qual now fails: the second action deletes it)
#  (b) the same, s1 aborts: s2's first action updates the original version
#  (c) s1 deleted the row: s2 waits, then takes the NOT MATCHED path (no such action: nothing)
#  (d) s1 moved the row to another partition (a cross-partition UPDATE): s2 waits, then raises PostgreSQL's
#      serialization failure for a tuple moved by a concurrent update
#  (e) a MATCHED DELETE after s1's update: s2 deletes the newest version
#  (f) two UPDATE actions of different SET lists (the host's cells their union): s1 updates a column the chosen action
#      does not set; s2's recheck keeps s1's value there (the carried cell is the newest version's, not the scan's)
# Expected output is PostgreSQL 18.6's (the C oracle).

setup
{
 CREATE TABLE rm (k int PRIMARY KEY, v int, note text) PARTITION BY RANGE (k);
 CREATE TABLE rm1 PARTITION OF rm FOR VALUES FROM (0) TO (100);
 CREATE TABLE rm2 PARTITION OF rm FOR VALUES FROM (100) TO (200);
 CREATE TABLE rm3 PARTITION OF rm DEFAULT;
 CREATE TABLE rms (k int, w int);
 INSERT INTO rm SELECT g, g % 50, 'i' FROM generate_series(0, 999) g;
 INSERT INTO rms VALUES (5, 1), (150, 2), (500, 3), (7, 4);
 ANALYZE rm, rms;
}

teardown
{
 DROP TABLE rm, rms;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1upd	{ UPDATE rm SET v = v + 100 WHERE k = 5; }
step s1del	{ DELETE FROM rm WHERE k = 5; }
step s1mv	{ UPDATE rm SET k = 2005 WHERE k = 5; }
step s1note	{ UPDATE rm SET note = 's1' WHERE k IN (7, 150); }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
setup
{
 SET enable_mergejoin = off;
 SET enable_nestloop = off;
}
step s2plan	{ EXPLAIN (costs off) MERGE INTO rm t USING rms s ON t.k = s.k WHEN MATCHED AND t.v < 100 THEN UPDATE SET v = t.v + s.w, note = 'm' WHEN MATCHED THEN DELETE; }
step s2merge	{ MERGE INTO rm t USING rms s ON t.k = s.k WHEN MATCHED AND t.v < 100 THEN UPDATE SET v = t.v + s.w, note = 'm' WHEN MATCHED THEN DELETE; }
step s2mdel	{ MERGE INTO rm t USING rms s ON t.k = s.k WHEN MATCHED AND s.w > 0 THEN DELETE; }
step s2two	{ MERGE INTO rm t USING rms s ON t.k = s.k WHEN MATCHED AND s.w = 4 THEN UPDATE SET v = t.v + 1000 WHEN MATCHED THEN UPDATE SET note = 'n' || s.w; }
step s2chk	{ SELECT tableoid::regclass, * FROM rm WHERE k IN (5, 7, 105, 150, 500, 2005) ORDER BY k; SELECT count(*), sum(v) FROM rm; }

permutation s2plan s1a
permutation s1upd s2merge s1c s2chk
permutation s1upd s2merge s1a s2chk
permutation s1del s2merge s1c s2chk
permutation s1mv s2merge s1c s2chk
permutation s1upd s2mdel s1c s2chk
permutation s1note s2two s1c s2chk

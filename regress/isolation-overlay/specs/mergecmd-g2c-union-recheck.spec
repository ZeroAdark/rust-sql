# mergecmd G2(c) part 2 (partition_prune #770 / #771 / #773 / #774): MERGE into a view with a check option over a
# partitioned table, the source a stable function's row joined to one leaf, so PostgreSQL 18.6 plans the result
# relations' Append (pruned at executor start) as a nested loop's outer and a Material over the leaf's scan as its inner
# (ROW_MARK_REFERENCE), while another session changes the matched row (READ COMMITTED). Its EvalPlanQual rechecks the
# row's own result relation at the newest version (the other result relations' scans blocked) and refetches the
# REFERENCE row the join read at its ctid, so the join qual and the view's qual run again over the newest version.
#  (a) s1 updates the row (b): deleted at its newest version, RETURNING its new b
#  (b) s1 makes the view's qual false on the newest version (b = 'a'): not deleted, nothing returned
#  (c) s1 deletes the row: skipped
#  (d) s1 moves the row's key within its leaf (a = 3): the join qual fails on the newest version: not deleted
#  (e) s1 aborts: the original version is deleted (control)
#  (f) the other leaf (another column order and a dropped column) with (a)
# Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE gu_abc (a int, b text, c bool) PARTITION BY LIST (a);
 CREATE TABLE gu_abc_1 (b text, a int, c bool);
 CREATE TABLE gu_abc_2 (a int, c bool, x int, b text);
 ALTER TABLE gu_abc_2 DROP COLUMN x;
 ALTER TABLE gu_abc ATTACH PARTITION gu_abc_1 FOR VALUES IN (1, 3);
 ALTER TABLE gu_abc ATTACH PARTITION gu_abc_2 FOR VALUES IN (2, 4);
 INSERT INTO gu_abc VALUES (1, 'b', true), (2, 'c', true);
 CREATE VIEW gu_view AS SELECT * FROM gu_abc WHERE b <> 'a' WITH CHECK OPTION;
 CREATE FUNCTION gu_one() RETURNS int AS $$ BEGIN RETURN 1; END; $$ LANGUAGE plpgsql STABLE;
}

teardown
{
 DROP VIEW gu_view;
 DROP TABLE gu_abc;
 DROP FUNCTION gu_one();
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1upd	{ UPDATE gu_abc SET b = 'x' WHERE a = 1; }
step s1qual	{ UPDATE gu_abc SET b = 'a' WHERE a = 1; }
step s1del	{ DELETE FROM gu_abc WHERE a = 1; }
step s1key	{ UPDATE gu_abc SET a = 3 WHERE a = 1; }
step s1upd2	{ UPDATE gu_abc SET b = 'y' WHERE a = 2; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
step s2exp	{ EXPLAIN (COSTS OFF) MERGE INTO gu_view pt USING (SELECT gu_one() AS pid) AS q JOIN gu_abc_1 pt1 ON (q.pid = pt1.a) ON pt.a = pt1.a WHEN MATCHED THEN DELETE RETURNING pt.a, pt.b, pt.c; }
step s2mrg	{ MERGE INTO gu_view pt USING (SELECT gu_one() AS pid) AS q JOIN gu_abc_1 pt1 ON (q.pid = pt1.a) ON pt.a = pt1.a WHEN MATCHED THEN DELETE RETURNING pt.a, pt.b, pt.c; }
step s2mrg2	{ MERGE INTO gu_view pt USING (SELECT gu_one() + 1 AS pid) AS q JOIN gu_abc_2 pt2 ON (q.pid = pt2.a) ON pt.a = pt2.a WHEN MATCHED THEN DELETE RETURNING pt.*, pt2.c; }
step s2sel	{ SELECT a, b, c FROM gu_abc ORDER BY a; }

permutation s2exp s1upd s2mrg s1c s2sel
permutation s1qual s2mrg s1c s2sel
permutation s1del s2mrg s1c s2sel
permutation s1key s2mrg s1c s2sel
permutation s1upd s2mrg s1a s2sel
permutation s1upd2 s2mrg2 s1c s2sel

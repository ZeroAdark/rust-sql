# lockrows J182 (regress join#182 / #183's form): FOR UPDATE OF t3, a nested loop's inner read through a plan Material,
# over an INNER merge join of t1 and the inheritance parent t2 (t2 and its child t2a, sorted under the merge join's inner
# Sort). t3 carries the lock; t1's PlanRowMark is REFERENCE, t2's isParent REFERENCE and each child's REFERENCE, gated on
# their family's tableoid column (EvalPlanQualFetchRowMark acts on a child's mark only where the row's tableoid names it).
# PostgreSQL's EvalPlanQual reruns the plan with t3's newest version as the Material child's test tuple and the REFERENCE
# rows refetched at their junk ctids (the version the statement read).
# Permutation 1: s1 bumps t3's row x = 5; s2's lock waits, then returns both join rows over the newest version (y 120).
# Permutation 2: s1 deletes t3's row x = 6; s2's rows over it drop.
# Permutation 3: s1 also changes the REFERENCE partner in child t2a (b 2001 -> 9) and moves t1's row a = 200 to a = 7; s2's
# recheck keeps the versions it read (t2a.b 2001, t1.a 200): the rows stay.
# t2's row and t2a's row sit at the same ctid (0,1) in their own heaps, so an ungated child joins its sibling's row too.
setup
{
  CREATE TABLE jt1 (a int, b int) WITH (autovacuum_enabled = off);
  CREATE TABLE jt2 (a int, b int) WITH (autovacuum_enabled = off);
  CREATE TABLE jt3 (x int, y int) WITH (autovacuum_enabled = off);
  CREATE TABLE jt2a () INHERITS (jt2);
  ALTER TABLE jt2a SET (autovacuum_enabled = off);
  INSERT INTO jt1 VALUES (5, 10), (15, 20), (100, 100), (200, 1000);
  INSERT INTO jt2 VALUES (200, 2000);
  INSERT INTO jt2a VALUES (100, 2001);
  INSERT INTO jt3 VALUES (5, 20), (6, 7);
}

teardown
{
  DROP TABLE jt2a, jt2, jt1, jt3;
}

session s1
step s1b { BEGIN; }
step s1bump { UPDATE jt3 SET y = y + 100 WHERE x = 5; }
step s1del { DELETE FROM jt3 WHERE x = 6; }
step s1refs { UPDATE jt2a SET b = 9; UPDATE jt1 SET a = 7 WHERE a = 200; UPDATE jt3 SET y = y + 100 WHERE x = 5; }
step s1c { COMMIT; }

session s2
setup { SET enable_hashjoin = off; }
step s2lk { SELECT jt1.*, jt2.*, u.* FROM jt1 JOIN jt2 ON (jt1.a = jt2.a), jt3 AS u FOR UPDATE OF u; }

permutation s1b s1bump s2lk s1c
permutation s1b s1del s2lk s1c
permutation s1b s1refs s2lk s1c

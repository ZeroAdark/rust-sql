# lockrows inherit#117: an UPDATE / DELETE whose IN (SELECT .. FROM an inheritance or partitioned parent) the planner
# unique-ifies with a HashAggregate over the parent's Append (no ANALYZE, as regress inherit.sql). The parent's mark is a
# REFERENCE mark (isParent; each child carries its own), and the aggregate carries its ctid / tableoid junk as any member
# of the group (PG: the first hashed tuple's): EvalPlanQual refetches that member at its junk ctid, in the child its junk
# tableoid names, at the version the statement read, and evaluates the join over it. It never re-runs the aggregate.
# Permutation 1: s1 bumps the ibar rows with f1 = 1 (partner ifoo (1, 1) at ctid (0,1)); s2's UPDATE waits, then rechecks
# each over its new version: f2 = 1 + 10 + 100.
# Permutation 2: s1 moves ibar's row (1, 1) to key 2. s2's recheck joins the new version with the one member it read
# (key 1): no match, the row is skipped. Key 2 has partners (ifoo2 (2, 2, 2), at ctid (0,1) too): a recheck that re-ran
# the aggregate, or fetched ctid (0,1) in every child, would update it.
# Permutation 3: s1 moves the partner itself (ifoo (1, 1) -> key 9) and bumps the target; s2's recheck reads the partner
# version it read (key 1): the update still applies.
# Permutation 4: the DELETE twin of permutation 2 (the moved row stays).
# Permutation 5: the partitioned twin (IN (SELECT a FROM ipt), partner (1, 10) in leaf ipt1 at ctid (0,1), ipt2's (0,1)
# holds key 3): s1 moves ibar2's (1, 1, 1) to key 3; s2's recheck skips it.
setup
{
  CREATE TABLE ifoo (f1 int, f2 int) WITH (autovacuum_enabled = off);
  CREATE TABLE ifoo2 (f3 int) INHERITS (ifoo);
  ALTER TABLE ifoo2 SET (autovacuum_enabled = off);
  CREATE TABLE ibar (f1 int, f2 int) WITH (autovacuum_enabled = off);
  CREATE TABLE ibar2 (f3 int) INHERITS (ibar);
  ALTER TABLE ibar2 SET (autovacuum_enabled = off);
  INSERT INTO ifoo VALUES (1, 1), (3, 3);
  INSERT INTO ifoo2 VALUES (2, 2, 2), (3, 3, 3);
  INSERT INTO ibar VALUES (1, 1), (2, 2), (3, 3), (4, 4);
  INSERT INTO ibar2 VALUES (1, 1, 1), (2, 2, 2), (3, 3, 3), (4, 4, 4);
  CREATE TABLE ipt (a int, b int) PARTITION BY RANGE (a);
  CREATE TABLE ipt1 PARTITION OF ipt FOR VALUES FROM (0) TO (3) WITH (autovacuum_enabled = off);
  CREATE TABLE ipt2 PARTITION OF ipt FOR VALUES FROM (3) TO (100) WITH (autovacuum_enabled = off);
  INSERT INTO ipt VALUES (1, 10), (3, 30), (2, 20), (5, 50);
}

teardown
{
  DROP TABLE ifoo, ibar, ipt CASCADE;
}

session s1
step s1b { BEGIN; }
step s1bump { UPDATE ibar SET f2 = f2 + 10 WHERE f1 = 1; }
step s1move { UPDATE ONLY ibar SET f1 = 2 WHERE f1 = 1; }
step s1part { UPDATE ONLY ifoo SET f1 = 9 WHERE f1 = 1; UPDATE ibar SET f2 = f2 + 10 WHERE f1 = 1; }
step s1movep { UPDATE ibar2 SET f1 = 3 WHERE f1 = 1; }
step s1c { COMMIT; }

session s2
step s2upd { UPDATE ibar SET f2 = f2 + 100 WHERE f1 IN (SELECT f1 FROM ifoo); }
step s2del { DELETE FROM ibar WHERE f1 IN (SELECT f1 FROM ifoo); }
step s2updp { UPDATE ibar SET f2 = f2 + 1000 WHERE f1 IN (SELECT a FROM ipt); }
step s2sel { SELECT tableoid::regclass::text AS rel, f1, f2 FROM ibar ORDER BY 1, 2, 3; }

permutation s1b s1bump s2upd s1c s2sel
permutation s1b s1move s2upd s1c s2sel
permutation s1b s1part s2upd s1c s2sel
permutation s1b s1move s2del s1c s2sel
permutation s1b s1movep s2updp s1c s2sel

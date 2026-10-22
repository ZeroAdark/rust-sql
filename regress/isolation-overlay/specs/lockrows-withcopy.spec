# lockrows WITHCOPY (with#285): a DELETE / UPDATE of an inheritance parent (one write per child) joined to a subquery row,
# its ROW_MARK_COPY row (wholerow), whose plan aggregates over the parent's own Append. PostgreSQL's EvalPlanQual returns
# the subquery's stored row at the Subquery Scan and runs nothing below it (the aggregate and its arms), then rechecks the
# target's newest version against that row.
# Permutation 1: s1 changes the max row's val (id stays 24): s2's DELETE waits, rechecks, deletes it.
# Permutation 2: s1 moves the max row's id (24 -> 25): s2's recheck finds id 25 against the stored maxid 24, deletes nothing.
# Permutation 3: the UPDATE twin of permutation 1 (SET val from the subquery row).
setup
{
  CREATE TABLE wp (id int, val text);
  CREATE TABLE wc1 () INHERITS (wp);
  CREATE TABLE wc2 () INHERITS (wp);
  INSERT INTO wp VALUES (1, 'p1');
  INSERT INTO wc1 VALUES (11, 'c11'), (12, 'c12');
  INSERT INTO wc2 VALUES (23, 'c21'), (24, 'c22');
}

teardown
{
  DROP TABLE wp CASCADE;
}

session s1
step s1b { BEGIN; }
step s1val { UPDATE wp SET val = val || 'x' WHERE id = 24; }
step s1move { UPDATE wp SET id = 25 WHERE id = 24; }
step s1c { COMMIT; }

session s2
step s2del { WITH r AS (SELECT max(id) AS maxid FROM wp) DELETE FROM wp USING r WHERE id = maxid; }
step s2upd { WITH r AS (SELECT max(id) AS maxid, count(*) AS n FROM wp) UPDATE wp SET val = val || '/' || r.n FROM r WHERE id = maxid; }
step s2sel { SELECT id, val FROM wp ORDER BY id; }

permutation s1b s1val s2del s1c s2sel
permutation s1b s1move s2del s1c s2sel
permutation s1b s1val s2upd s1c s2sel

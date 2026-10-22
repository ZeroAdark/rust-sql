# lockrows REFPARENT: an UPDATE / DELETE / MERGE joined to an inheritance or partitioned parent that is not its target. The
# parent's PlanRowMark is isParent (a REFERENCE mark); each child carries its own, and EvalPlanQualFetchRowMark fetches a
# child's row only when the child is the row's tableoid (the junk tableoid column): the other children give no row. So the
# recheck over the target's newest version reads the one referenced row, from its own child, at its junk ctid (the
# version the statement read: a REFERENCE mark never takes the newer one).
# Permutation 1: s1 bumps the target rows a = 40 (partner in rp) and a = 45 (partner in child rc); s2's UPDATE waits on
# them, then rechecks each over its new version: b = 1 + 100 + the partner's v (501, 551).
# Permutation 2: s1 moves the target row's key (a = 45 -> 1045); s2's recheck finds no partner: the row is skipped.
# Permutation 3: s1 changes the partner row in child rc; s2 waits on the target row it also bumped, then rechecks with the
# partner version it read (v 450, not 9): the update still applies with the old v.
# Permutation 4: the DELETE over a partitioned parent (partner in leaf rq2); s1 bumps the target; s2's recheck deletes it.
# Permutation 5: the MERGE twin of permutation 1 (its ON clause keeps partners with v 440..460: only row 45 merges).
# Permutations 6, 7 (D3400 REFPAR2): the UPDATE joined to two parents, rp's family and rq's, each child gated on its own
# family's tableoid column: permutation 1's and 3's rechecks, each row's rq partner (rq2's 40, 45) added too. rq's own
# filter keeps the target's scan the outer of the top hash join (rp's and rq's join hashed); without it PostgreSQL 18.6
# hashes rt's join to rp and probes it with rq's Append, a target on a hash build side under a family's arms (D3403).
# Every permutation: each partner's ctid holds a dead row with the same key in the other heap: rp keeps (45, 420) at the
# ctid of rc's row 45, (0,5); rc keeps (40, 405) at the ctid of rp's row 40, (0,40). The recheck refetches a partner at its
# junk ctid only in the child the junk tableoid names: a dead row never joins (501 / 551, not 506 / 521).
setup
{
  CREATE TABLE rp (id int, v int) WITH (autovacuum_enabled = off);
  CREATE TABLE rc () INHERITS (rp);
  ALTER TABLE rc SET (autovacuum_enabled = off);
  INSERT INTO rp SELECT g, g * 10 FROM generate_series(1, 4) g;
  INSERT INTO rp VALUES (45, 420);
  INSERT INTO rp SELECT g, g * 10 FROM generate_series(6, 40) g;
  DELETE FROM rp WHERE id = 45;
  INSERT INTO rc SELECT g, g * 10 FROM generate_series(41, 79) g;
  INSERT INTO rc VALUES (40, 405);
  INSERT INTO rc VALUES (80, 800);
  DELETE FROM rc WHERE id = 40;
  CREATE TABLE rq (id int, v int) PARTITION BY RANGE (id);
  CREATE TABLE rq1 PARTITION OF rq FOR VALUES FROM (0) TO (40);
  CREATE TABLE rq2 PARTITION OF rq FOR VALUES FROM (40) TO (1000);
  INSERT INTO rq SELECT g, g FROM generate_series(1, 80) g;
  CREATE TABLE rt (a int, b int);
  INSERT INTO rt SELECT g, 1 FROM generate_series(1, 90) g;
  ANALYZE rp, rc, rq, rt;
}

teardown
{
  DROP TABLE rt, rc, rp, rq;
}

session s1
step s1b { BEGIN; }
step s1bump { UPDATE rt SET b = b + 100 WHERE a IN (40, 45); }
step s1move { UPDATE rt SET a = a + 1000 WHERE a = 45; }
step s1part { UPDATE rc SET v = 9 WHERE id = 45; UPDATE rt SET b = b + 100 WHERE a IN (40, 45); }
step s1c { COMMIT; }

session s2
setup { SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2upd { UPDATE rt SET b = b + rp.v FROM rp WHERE rt.a = rp.id AND rp.v >= 400 AND rp.v <= 460; }
step s2del { DELETE FROM rt USING rq WHERE rt.a = rq.id AND rq.v BETWEEN 44 AND 46; }
step s2mrg { MERGE INTO rt USING rp ON rt.a = rp.id AND rp.v BETWEEN 440 AND 460 WHEN MATCHED THEN UPDATE SET b = rt.b + rp.v; }
step s2two { UPDATE rt SET b = b + rp.v + rq.v FROM rp, rq WHERE rt.a = rp.id AND rt.a = rq.id AND rp.v >= 400 AND rp.v <= 460 AND rq.v BETWEEN 38 AND 47; }
step s2sel { SELECT a, b FROM rt WHERE a BETWEEN 40 AND 50 OR a > 1000 ORDER BY a; }

permutation s1b s1bump s2upd s1c s2sel
permutation s1b s1move s2upd s1c s2sel
permutation s1b s1part s2upd s1c s2sel
permutation s1b s1bump s2del s1c s2sel
permutation s1b s1bump s2mrg s1c s2sel
permutation s1b s1bump s2two s1c s2sel
permutation s1b s1part s2two s1c s2sel

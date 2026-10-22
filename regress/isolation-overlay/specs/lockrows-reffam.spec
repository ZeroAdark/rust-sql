# lockrows REFFAM: a row lock on one inheritance family beside another parent's whole REFERENCE family, over a hash join
# (each family's Append kept as a settled union; the recheck reruns both unions' feeds into its own empty spools). The
# locked family's parent and children carry the lock; the other parent's PlanRowMark is isParent REFERENCE and each of
# its children REFERENCE. Each family's children share their parent's junk ctid and tableoid columns, and ExecLockRows /
# EvalPlanQualFetchRowMark act on a child's mark only where the row's tableoid names that child: the other children are
# inactive (no lock, no EPQ test tuple).
# Permutation 1: s1 bumps the locked rows id 40 (in lu) and 45 (in child luc); s2's FOR UPDATE OF a waits, then rechecks
# each over its newest version (v 140, 145) with the REFERENCE partner refetched at its junk ctid in its own child.
# Permutation 2: s1 also changes the partner row 45 in child rc (v 450 -> 9); s2's recheck keeps the partner version it
# read (v 450): the row stays.
# Permutation 3: the families swap roles (portals#244's form: the locked family's union feeds the hash build, the
# REFERENCE family's union is read by the lock): s2 locks rp's family (FOR SHARE OF b) after s1 bumps rp rows 40 and 45;
# lu's family is the REFERENCE one. Permutation 4: s1 moves rp's row 45 out of s2's range (v 450 -> 9): the recheck's build
# over its newest version is empty for that row, which drops.
# Permutations 5 / 6 (portals#244's form: FOR UPDATE of both aliases, both families locked): after s1 bumps lu rows 40 and
# 45, s2 returns them over their newest versions; after s1 moves rp's row 45 out of range, that row drops.
# Every permutation: each row's ctid holds a dead row with the same key in the other heap of its family: rp keeps (45, 420)
# at the ctid of rc's row 45, (0,5), rc keeps (40, 405) at the ctid of rp's row 40, (0,40); lu keeps (45, 999) at (0,5),
# luc keeps (40, 999) at (0,40). An ungated child refetches that dead row and joins it.
setup
{
  CREATE TABLE lu (id int, v int) WITH (autovacuum_enabled = off);
  CREATE TABLE luc () INHERITS (lu);
  ALTER TABLE luc SET (autovacuum_enabled = off);
  INSERT INTO lu SELECT g, g FROM generate_series(1, 4) g;
  INSERT INTO lu VALUES (45, 999);
  INSERT INTO lu SELECT g, g FROM generate_series(6, 40) g;
  DELETE FROM lu WHERE id = 45;
  INSERT INTO luc SELECT g, g FROM generate_series(41, 79) g;
  INSERT INTO luc VALUES (40, 999);
  INSERT INTO luc VALUES (80, 80);
  DELETE FROM luc WHERE id = 40;
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
  ANALYZE lu, luc, rp, rc;
}

teardown
{
  DROP TABLE luc, lu, rc, rp;
}

session s1
step s1b { BEGIN; }
step s1bumpa { UPDATE lu SET v = v + 100 WHERE id IN (40, 45); }
step s1part { UPDATE rc SET v = 9 WHERE id = 45; UPDATE lu SET v = v + 100 WHERE id IN (40, 45); }
step s1bumpb { UPDATE rp SET v = v + 1 WHERE id IN (40, 45); }
step s1outb { UPDATE rp SET v = 9 WHERE id = 45; }
step s1c { COMMIT; }

session s2
setup { SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2lka { SELECT a.id, a.v, b.v AS bv FROM lu a, rp b WHERE a.id = b.id AND b.v BETWEEN 400 AND 460 FOR UPDATE OF a; }
step s2lkb { SELECT a.id, a.v, b.v AS bv FROM lu a, rp b WHERE a.id = b.id AND b.v BETWEEN 400 AND 460 FOR SHARE OF b; }
step s2lkab { SELECT a.id, a.v, b.v AS bv FROM lu a, rp b WHERE a.id = b.id AND b.v BETWEEN 400 AND 460 FOR UPDATE; }

permutation s1b s1bumpa s2lka s1c
permutation s1b s1part s2lka s1c
permutation s1b s1bumpb s2lkb s1c
permutation s1b s1outb s2lkb s1c
permutation s1b s1bumpa s2lkab s1c
permutation s1b s1outb s2lkab s1c

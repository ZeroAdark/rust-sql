# lockrows TIDINIT (gen queue-0007): LockRows over a TID scan whose TID qual reads an InitPlan (j.ctid = (SELECT ctid FROM
# tj WHERE id = 5) .. FOR UPDATE). The TID scan evaluates its TID quals at its first fetch (TidListEval), so the InitPlan
# ran before the row reached the lock; EvalPlanQualStart copies its value, and the recheck (TidRecheck) tests the newer
# version's ctid against that TID list. s1 updates or deletes row 5 (its newer version is at another ctid): s2's recheck
# fails and LockRows skips the row. s1 only locking row 5: s2 waits, then locks the unchanged row. SKIP LOCKED skips a row
# s1 holds; NOWAIT raises 55P03; the array form keeps the other rows. Expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE tj (id int PRIMARY KEY, state text);
  INSERT INTO tj SELECT g, 'ready' FROM generate_series(1, 10) g;
}
teardown { DROP TABLE tj; }

session s1
setup		{ BEGIN; }
step s1u	{ UPDATE tj SET state = 'taken' WHERE id = 5; }
step s1d	{ DELETE FROM tj WHERE id = 5; }
step s1k	{ SELECT id FROM tj WHERE id = 5 FOR KEY SHARE; }
step s1c	{ COMMIT; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) SELECT j.id, j.state FROM tj j WHERE j.ctid = (SELECT ctid FROM tj WHERE id = 5) FOR UPDATE; }
step s2l	{ SELECT j.id, j.state FROM tj j WHERE j.ctid = (SELECT ctid FROM tj WHERE id = 5) FOR UPDATE; }
step s2s	{ SELECT j.id, j.state FROM tj j WHERE j.ctid = (SELECT ctid FROM tj WHERE id = 5) FOR UPDATE SKIP LOCKED; }
step s2n	{ SELECT j.id, j.state FROM tj j WHERE j.ctid = (SELECT ctid FROM tj WHERE id = 5) FOR UPDATE NOWAIT; }
step s2a	{ SELECT j.id, j.state FROM tj j WHERE j.ctid = ANY (ARRAY(SELECT ctid FROM tj WHERE id IN (4, 5, 6))) FOR UPDATE; }

permutation s2x s1u s2l s1c
permutation s1d s2l s1c
permutation s1k s2l s1c
permutation s1u s2s s1c
permutation s1u s2n s1c
permutation s1u s2a s1c

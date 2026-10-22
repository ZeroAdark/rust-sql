# PUBLISH-WAIT (pgrc-plans/PUBLISH-WAIT-design.md section 5.2), CONCLOAD (pgrc-plans/CONCLOAD.done), LOADWAIT
# (pgrc-plans/LOADWAIT.done): bulk loads no longer wait for each other, so opposite-order loads of two tables do not
# deadlock, and a loader that then builds an index waits for the other load's RowExclusiveLock only, as with heap, and
# then builds it over both loads (IX1 L1, the pgrcolumnar index build scan: da = 1000; until IX1 L1 the build was
# refused and da = 500). A TRUNCATE activates the front segment (LOADWAIT), so a small insert
# after it publishes nothing and a load beside it does not wait either (until LOADWAIT that insert's activation made
# the load wait, and a LOCK TABLE ... SHARE closed a 40P01). A publish wait that remains (a load behind a pgrc_flush
# held open: its rotation and seal change the segment records, which a load does not stack over) is a transactionid
# lock wait: the deadlock detector sees it, and a cycle through it is a detected, retryable 40P01, never a hang (the
# suite timeout is the hang detector). A backend runs its deadlock check once, deadlock_timeout into its wait, so the
# session that closes the cycle (s1's LOCK TABLE) has the short timeout (1 s) and is the victim; s2, waiting first,
# has 100 s and never checks before the cycle is gone. The heap twin never blocks on a publish and builds the index:
# the expected output is the overlay's.

setup { CREATE TABLE pgrc_da (id int8) USING pgrcolumnar; CREATE TABLE pgrc_db (id int8) USING pgrcolumnar; }
setup { TRUNCATE pgrc_da; }
teardown { DROP TABLE pgrc_da, pgrc_db; }

session s1
setup       { SET pgrust.pgrc_bulk_rows = 100; SET deadlock_timeout = '1s'; }
step s1b    { BEGIN; }
step s1a    { INSERT INTO pgrc_da SELECT generate_series(1, 500); }
step s1bb   { INSERT INTO pgrc_db SELECT generate_series(1, 500); }
step s1f    { INSERT INTO pgrc_da VALUES (0); }
step s1flush { SELECT pgrc_flush('pgrc_da'); }
step s1idx  { CREATE INDEX pgrc_da_id ON pgrc_da (id); }
step s1lock { LOCK TABLE pgrc_da IN SHARE MODE; }
step s1c    { COMMIT; }

session s2
setup       { SET pgrust.pgrc_bulk_rows = 100; SET deadlock_timeout = '100s'; }
step s2b    { BEGIN; }
step s2bb   { INSERT INTO pgrc_db SELECT generate_series(1001, 1500); }
step s2a    { INSERT INTO pgrc_da SELECT generate_series(1001, 1500); }
step s2f    { INSERT INTO pgrc_da VALUES (0); }
step s2c    { COMMIT; }

session r
# two plain counts: with the SQE slot at its default the scalar-subquery pair was an ERROR (SQE: "not served",
# shape/result/gate-child), not a fallback
step rc     { SELECT count(*) AS da FROM pgrc_da; SELECT count(*) AS db FROM pgrc_db; }

# opposite order: each load publishes over the other's, both commit
permutation s1b s1a s2b s2bb s1bb s2a s1c s2c rc
# the first loader builds an index while the second is open: it waits for s2's commit, then builds it (da = 1000)
permutation s1b s1a s2b s2a s1idx s2c s1c rc
# s1's small insert after the TRUNCATE (its front segment already active) does not hold off s2's load, so s1's LOCK
# TABLE ... IN SHARE MODE finds no RowExclusiveLock left and no cycle forms: da = 501, as on heap
permutation s1b s1f s2a s1lock s1c rc
# the residual: s1's pgrc_flush seals s2's committed front row in s1's transaction; s2's load waits for s1; s1's LOCK
# TABLE ... IN SHARE MODE closes the cycle and gets 40P01; s1's abort ends the flush and s2's load goes through
permutation s2f s1b s1flush s2a s1lock s1c rc

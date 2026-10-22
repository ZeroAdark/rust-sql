# pgrc F2 L5 (docs/design/pgrc-delta-v2/F2.md), OC-4: write skew under
# SERIALIZABLE. Each transaction reads the rows the other writes and writes a
# row the other read; one of them must fail with 40001. pgrc takes its
# predicate locks at relation grain (PredicateLockRelation at filter build,
# CheckForSerializableConflictIn before each tombstone); the heap twin's
# sequential scans lock the relation too, so the twin is the expected output
# (scripts/pgrc-isolation.sh). A permutation whose relation-grain answer
# differs from the twin gets its own expected file in
# regress/isolation-overlay/expected/. Rows 1 and 2 are in the sealed part, 11
# and 12 in the live segment.

setup { CREATE TABLE pgrc_t (id int, oncall bool, grp int) USING pgrcolumnar; }
setup { INSERT INTO pgrc_t SELECT g, true, g % 10 FROM generate_series(1, 10) g; }
setup { CALL pgrc_seal('pgrc_t'); }
setup { INSERT INTO pgrc_t SELECT g, true, g % 10 FROM generate_series(11, 20) g; }

teardown { DROP TABLE pgrc_t; }

session s1
setup       { BEGIN ISOLATION LEVEL SERIALIZABLE; }
step s1r    { SELECT count(*) FROM pgrc_t WHERE oncall AND grp IN (1, 2); }
step s1wp   { UPDATE pgrc_t SET oncall = false WHERE id = 1; }
step s1ws   { UPDATE pgrc_t SET oncall = false WHERE id = 11; }
step s1d    { DELETE FROM pgrc_t WHERE id = 11; }
step s1i    { INSERT INTO pgrc_t VALUES (21, true, 1); }
step s1c    { COMMIT; }

session s2
setup       { BEGIN ISOLATION LEVEL SERIALIZABLE; }
step s2r    { SELECT count(*) FROM pgrc_t WHERE oncall AND grp IN (1, 2); }
step s2wp   { UPDATE pgrc_t SET oncall = false WHERE id = 2; }
step s2ws   { UPDATE pgrc_t SET oncall = false WHERE id = 12; }
step s2d    { DELETE FROM pgrc_t WHERE id = 12; }
step s2i    { INSERT INTO pgrc_t VALUES (22, true, 2); }
step s2c    { COMMIT; }

session r
step rsel   { SELECT id, oncall FROM pgrc_t WHERE grp IN (1, 2) ORDER BY id; }

# write skew, part rows / segment rows / one of each / by DELETE
permutation s1r s2r s1wp s2wp s1c s2c rsel
permutation s1r s2r s1ws s2ws s1c s2c rsel
permutation s1r s2r s1wp s2ws s1c s2c rsel
permutation s1r s2r s1d s2d s1c s2c rsel
# by INSERT: each inserts a row the other's read would have counted (heap_insert's relation-grain check)
permutation s1r s2r s1i s2i s1c s2c rsel
# serial: no failure
permutation s1r s1wp s1c s2r s2wp s2c rsel

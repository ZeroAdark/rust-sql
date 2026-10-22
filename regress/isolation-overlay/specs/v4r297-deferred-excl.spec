# V4R-297: a served INSERT into a table with a DEFERRABLE exclusion constraint probes it without waiting (CEOUC_NOWAIT,
# violation allowed) and queues the recheck; the recheck (unique_key_recheck: check_exclusion_constraint, waiting) runs at
# COMMIT or SET CONSTRAINTS ... IMMEDIATE, and waits out a concurrent in-progress conflicting inserter there. Two sessions
# insert the same key: both inserts return at once; the outcome is decided at commit / rollback / SET CONSTRAINTS.
setup
{
 CREATE TABLE v297x (k int, s text, CONSTRAINT v297x_c EXCLUDE (k WITH =) DEFERRABLE INITIALLY DEFERRED);
 INSERT INTO v297x SELECT g, 'seed' FROM generate_series(1, 10) g;
}
teardown { DROP TABLE v297x; }
session s1
setup { BEGIN ISOLATION LEVEL READ COMMITTED; }
step i1 { INSERT INTO v297x VALUES (100, 's1'); }
step c1 { COMMIT; }
step r1 { ROLLBACK; }
session s2
setup { BEGIN ISOLATION LEVEL READ COMMITTED; }
step i2 { INSERT INTO v297x VALUES (100, 's2'); }
step imm2 { SET CONSTRAINTS ALL IMMEDIATE; }
step c2 { COMMIT; }
step r2 { ROLLBACK; }
session s3
step read { SELECT k, s FROM v297x WHERE k >= 100 ORDER BY k, s; }
permutation i1 c1 i2 c2 read
permutation i1 i2 r2 c1 read
permutation i1 i2 r1 c2 read
permutation i1 i2 imm2 r1 c2 read

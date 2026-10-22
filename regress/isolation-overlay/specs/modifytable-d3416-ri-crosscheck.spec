# D3416 RI-CROSSCHECK (E1's isolation NEW-REFUSAL fk-snapshot, road R1): an RI action's DELETE / UPDATE (ON DELETE
# CASCADE / SET NULL) under a transaction snapshot runs with a crosscheck snapshot (ri_PerformCheck's
# SPI_execute_snapshot). The write hosts hand it to heap_delete / heap_update, so a referencing row another
# transaction committed after this one's snapshot is a serialization failure, as PostgreSQL's ExecDelete /
# ExecUpdate report it: a plain referencing table, a partitioned one (one result relation per partition) and SET NULL.
# A row committed before the snapshot is deleted or set to NULL as usual.

setup
{
  CREATE TABLE rc_pk (a int PRIMARY KEY);
  CREATE TABLE rc_cas (a int REFERENCES rc_pk ON DELETE CASCADE, n text);
  CREATE TABLE rc_casp (a int REFERENCES rc_pk ON DELETE CASCADE, n text) PARTITION BY LIST (a);
  CREATE TABLE rc_casp1 PARTITION OF rc_casp FOR VALUES IN (1, 2);
  CREATE TABLE rc_casp2 PARTITION OF rc_casp FOR VALUES IN (3, 4);
  CREATE TABLE rc_sn (a int REFERENCES rc_pk ON DELETE SET NULL, n text);
  INSERT INTO rc_pk VALUES (1), (2), (3), (4);
  INSERT INTO rc_cas VALUES (1, 'c1');
  INSERT INTO rc_casp VALUES (1, 'p1'), (3, 'p3');
  INSERT INTO rc_sn VALUES (1, 's1');
}

teardown
{
  DROP TABLE rc_cas, rc_casp, rc_sn, rc_pk;
}

session s1
step s1b	{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1ic	{ INSERT INTO rc_cas VALUES (2, 'c2'); }
step s1ip	{ INSERT INTO rc_casp VALUES (4, 'p4'); }
step s1is	{ INSERT INTO rc_sn VALUES (2, 's2'); }
step s1c	{ COMMIT; }
step s1s	{ SELECT 'cas' AS t, a, n FROM rc_cas UNION ALL SELECT 'casp', a, n FROM rc_casp UNION ALL SELECT 'sn', a, n FROM rc_sn ORDER BY 1, 2, 3; }

session s2
step s2rr	{ BEGIN ISOLATION LEVEL REPEATABLE READ; }
step s2d1	{ DELETE FROM rc_pk WHERE a = 1; }
step s2d2	{ DELETE FROM rc_pk WHERE a = 2; }
step s2d4	{ DELETE FROM rc_pk WHERE a = 4; }
step s2c	{ COMMIT; }

# a CASCADE onto a plain table: s1's row came after s2's snapshot
permutation s1b s1ic s2rr s2d2 s1c s2c s1s
# a CASCADE onto a partitioned table
permutation s1b s1ip s2rr s2d4 s1c s2c s1s
# a SET NULL
permutation s1b s1is s2rr s2d2 s1c s2c s1s
# s1 committed before s2's snapshot: deleted / set to NULL
permutation s1b s1ic s1ip s1is s1c s2rr s2d2 s2d4 s2c s1s
# the setup's rows: CASCADE on both tables and SET NULL
permutation s2rr s2d1 s2c s1s

# D727: an UPDATE whose table has a BEFORE UPDATE row trigger locks the old row with ExecUpdateLockMode (PostgreSQL
# 18.6 trigger.c GetTupleForTrigger, nodeModifyTable.c ExecUpdateLockMode): LockTupleNoKeyExclusive unless the UPDATE
# changes a key column, so a FOR KEY SHARE NOWAIT reader in another session gets the row. pgrust's row executor locked
# it LockTupleExclusive, so the reader failed 55P03 (fixed by e3c75df1c03). The filing form reached the row executor
# through a partitioned root; the plain table is the same path. DOES NOT APPLY: an UPDATE that changes the key takes the
# exclusive lock, so the reader fails 55P03 in PostgreSQL too.
setup
{
 CREATE TABLE d727_t (k int PRIMARY KEY, v int);
 INSERT INTO d727_t VALUES (1, 1), (2, 2);
 CREATE FUNCTION d727_tf() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
 CREATE TRIGGER d727_bu BEFORE UPDATE ON d727_t FOR EACH ROW EXECUTE FUNCTION d727_tf();
 CREATE TABLE d727_p (k int PRIMARY KEY, v int) PARTITION BY RANGE (k);
 CREATE TABLE d727_p1 PARTITION OF d727_p FOR VALUES FROM (0) TO (100);
 CREATE TRIGGER d727_pbu BEFORE UPDATE ON d727_p FOR EACH ROW EXECUTE FUNCTION d727_tf();
 INSERT INTO d727_p VALUES (1, 1), (2, 2);
}
teardown { DROP TABLE d727_t, d727_p; DROP FUNCTION d727_tf(); }
session s1
setup { BEGIN; }
step u_plain { UPDATE d727_t SET v = v + 1 WHERE k = 1; }
step u_part { UPDATE d727_p SET v = v + 1 WHERE k = 1; }
step u_key { UPDATE d727_t SET k = 10 WHERE k = 1; }
step c1 { COMMIT; }
session s2
setup { BEGIN; }
step ks_plain { SELECT k, v FROM d727_t WHERE k = 1 FOR KEY SHARE NOWAIT; }
step ks_part { SELECT k, v FROM d727_p WHERE k = 1 FOR KEY SHARE NOWAIT; }
step c2 { COMMIT; }
session s3
step read { SELECT 't' AS rel, * FROM d727_t UNION ALL SELECT 'p', * FROM d727_p ORDER BY 1, 2; }
permutation u_plain ks_plain c1 c2 read
permutation u_part ks_part c1 c2 read
permutation u_key ks_plain c1 c2 read

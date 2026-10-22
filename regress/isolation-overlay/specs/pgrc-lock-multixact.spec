# LOCK (pgrc delta-store, 2026-10-03): multixact row locks and deadlocks on a pgrcolumnar table, PG's
# multixact-no-deadlock and tuplelock-upgrade-no-deadlock on a twin (one row: id 1 in a sealed part), plus a classic
# lock-upgrade deadlock (two FOR SHARE holders that both UPDATE: one is the deadlock victim, as on heap). Expected
# output: the heap twin (scripts/pgrc-isolation.sh).

setup { CREATE TABLE tlu_job (id integer, name text) USING pgrcolumnar; }
setup { INSERT INTO tlu_job VALUES (1, 'a'); }
setup { CALL pgrc_seal('tlu_job'); }

teardown { DROP TABLE tlu_job; }

session s1
setup { BEGIN; }
step s1_keyshare { select id from tlu_job where id = 1 for key share;}
step s1_share { select id from tlu_job where id = 1 for share; }
step s1_svpt { savepoint foo; }
step s1_update { update tlu_job set name = 'b' where id = 1;  }
step s1_rollback { rollback; }
step s1_commit { commit; }

session s2
setup { begin; }
step s2_share { select id from tlu_job where id = 1 for share; }
step s2_for_update { select id from tlu_job where id = 1 for update; }
step s2_update { update tlu_job set name = 'b' where id = 1; }
step s2_rollback { rollback; }
step s2_commit { commit; }

session s3
setup { begin; }
step s3_keyshare { select id from tlu_job where id = 1 for key share; }
step s3_share { select id from tlu_job where id = 1 for share; }
step s3_for_update { select id from tlu_job where id = 1 for update; }
step s3_update { update tlu_job set name = 'c' where id = 1; }
step s3_delete { delete from tlu_job where id = 1; }
step s3_rollback { rollback; }
step s3_commit { commit; }

session r
step rsel { SELECT * FROM tlu_job ORDER BY id; }

# multixact-no-deadlock
permutation s1_share s2_share s1_svpt s3_for_update s1_share s2_commit s1_commit s3_commit rsel
# tuplelock-upgrade-no-deadlock
permutation s1_share s2_for_update s3_share s3_for_update s1_rollback s3_rollback s2_rollback rsel
permutation s1_keyshare s2_for_update s3_keyshare s1_update s3_update s1_rollback s3_rollback s2_rollback rsel
permutation s1_keyshare s2_for_update s3_keyshare s1_update s3_update s1_commit s3_rollback s2_rollback rsel
permutation s1_keyshare s2_for_update s3_keyshare s3_delete s1_rollback s3_rollback s2_rollback rsel
permutation s1_keyshare s2_for_update s3_keyshare s3_delete s1_rollback s3_commit s2_rollback rsel
permutation s1_share s2_for_update s3_for_update s1_rollback s2_rollback s3_rollback rsel
permutation s1_share s2_update s3_update s1_rollback s2_rollback s3_rollback rsel
# deadlock: both share, both update
permutation s1_share s2_share s1_update s2_update s1_commit s2_rollback rsel

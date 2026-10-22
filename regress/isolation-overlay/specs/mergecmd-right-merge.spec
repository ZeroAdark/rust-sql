# mergecmd RIGHT-MERGE (merge#468 group): a MERGE with NOT MATCHED actions PostgreSQL 18.6 runs over a Merge Right Join
# (tables never analyzed: the target's Sort the outer, the source's Sort the inner, an unmatched source row coming out
# with a NULL target). s1 changes the matched target row tid = 3 and the MERGE waits on it; at s1's commit the recheck
# reruns the target's sorted producer at the newest version and the source's at the row the join read: an updated row
# stays MATCHED (n), a re-keyed (k) or deleted (d) one no longer joins and its source row is NOT MATCHED (inserted), as
# EvalPlanQual's null-extended row makes ExecMergeMatched hand it to the NOT MATCHED actions. s3 is the source a merge
# join of two relations under a Materialize (merge#470's plan).

# FULL-OUTER (regress merge#56's form): s2f adds a NOT MATCHED BY SOURCE action, so PostgreSQL plans a Merge Full Join, the
# target's Sort still the outer: s1 changes the matched row tid = 3 as above, or updates (u9) or deletes (d9) the BY SOURCE
# row tid = 9 (EvalPlanQual's join over its newest version and no source row: BY SOURCE again; deleted: nothing). The
# re-keyed row (s1k) runs s2fn, s2f without RETURNING: its recheck runs two actions (the BY SOURCE UPDATE of the newest
# version, then the source row's NOT MATCHED INSERT), and the order of their two RETURNING rows is plan-dependent with no
# ORDER BY (the order / multiset ruling: same multiset, any order), so the writes and the final rows are what is compared.
# RIGHT_OWN (held-out subq-0184): the o-steps run MATCHED beside NOT MATCHED over a Merge Right Join whose outer is the
# target's own index scan (ro_t analyzed), the source's Sort the inner; source rows with no target row come out of the inner
# fill with a NULL target before, between and after the matched ones (5001 after the outer's end). s1 changes the matched
# row id = 10: updated (on), its UPDATE reads the newest version; re-keyed (ok) or deleted (od), its source row takes the
# NOT MATCHED INSERT; (oq) the newest version fails the action qual: no action. s2oq's UPDATE also reads a SubPlan
# correlated to the target row (subq-0184's form).

# D2744: the h-steps run a MERGE with MATCHED UPDATE, NOT MATCHED INSERT and NOT MATCHED BY SOURCE UPDATE actions over a
# Hash Full Join (hf_t analyzed and large, the source hashed; merge and nested-loop joins off). s1 re-keys the matched row
# 3 to 30001: PostgreSQL runs the BY SOURCE UPDATE on 30001 and then the source row 3's INSERT. s4n has no RETURNING;
# s4r returns both rows (ExecMerge's pending NOT MATCHED row: D2744, the recheck's second row). The source leaves one
# drained row (6), so the RETURNING order is PostgreSQL's (drained rows come in hash-table order).

setup
{
  CREATE TABLE rm_t (tid int, balance int, val text) WITH (autovacuum_enabled = off);
  CREATE TABLE rm_s (sid int, delta int) WITH (autovacuum_enabled = off);
  CREATE TABLE rm_s2 (sid2 int, sval text) WITH (autovacuum_enabled = off);
  INSERT INTO rm_t SELECT i, i * 100, 'initial' FROM generate_series(1, 9, 2) i;
  INSERT INTO rm_s SELECT i, i * 10 FROM generate_series(1, 6) i;
  INSERT INTO rm_s2 SELECT i, 'src2 ' || i FROM generate_series(1, 6) i;
  CREATE TABLE ro_t (id int PRIMARY KEY, n int NOT NULL, val text);
  INSERT INTO ro_t SELECT i * 2, i, 'v' || i FROM generate_series(1, 2000) i;
  CREATE TABLE ro_s (id int, n int);
  INSERT INTO ro_s VALUES (1, 1), (3, 3), (4, 40), (6, 3), (10, 50), (11, 11), (5001, 7);
  CREATE TABLE ro_c (id int);
  INSERT INTO ro_c VALUES (4), (10), (10), (12);
  CREATE TABLE hf_t (tid int, balance int, val text);
  INSERT INTO hf_t SELECT i, i * 100, 'initial' FROM generate_series(1, 4000, 2) i;
  CREATE TABLE hf_s (sid int, delta int);
  INSERT INTO hf_s SELECT i, i * 10 FROM generate_series(1, 6) i WHERE i NOT IN (2, 4);
}
setup { VACUUM ANALYZE ro_t; }
setup { VACUUM ANALYZE ro_s; }
setup { VACUUM ANALYZE ro_c; }
setup { ANALYZE hf_t; }
setup { ANALYZE hf_s; }

teardown
{
  DROP TABLE rm_t, rm_s, rm_s2, ro_t, ro_s, ro_c, hf_t, hf_s;
}

session s1
setup { BEGIN; }
step s1n { UPDATE rm_t SET balance = balance + 1 WHERE tid = 3; }
step s1k { UPDATE rm_t SET tid = 30 WHERE tid = 3; }
step s1d { DELETE FROM rm_t WHERE tid = 3; }
step s1u9 { UPDATE rm_t SET balance = balance + 1 WHERE tid = 9; }
step s1d9 { DELETE FROM rm_t WHERE tid = 9; }
step s1on { UPDATE ro_t SET n = n + 1, val = val || ' s1' WHERE id = 10; }
step s1ok { UPDATE ro_t SET id = 9 WHERE id = 10; }
step s1od { DELETE FROM ro_t WHERE id = 10; }
step s1oq { UPDATE ro_t SET n = 50 WHERE id = 10; }
step s1hk { UPDATE hf_t SET tid = 30001 WHERE tid = 3; }
step s1c { COMMIT; }
step s1a { ROLLBACK; }

session s2
setup { SET enable_hashjoin = off; SET enable_nestloop = off; }
step s2e { EXPLAIN (COSTS OFF) MERGE INTO rm_t t USING rm_s s ON t.tid = s.sid WHEN MATCHED THEN UPDATE SET balance = balance + delta, val = val || ' updated' WHEN NOT MATCHED THEN INSERT VALUES (sid, delta, 'inserted'); }
step s2m { MERGE INTO rm_t t USING rm_s s ON t.tid = s.sid WHEN MATCHED THEN UPDATE SET balance = balance + delta, val = val || ' updated' WHEN NOT MATCHED THEN INSERT VALUES (sid, delta, 'inserted'); }
step s2x { MERGE INTO rm_t t USING rm_s s ON t.tid = s.sid WHEN MATCHED THEN DELETE WHEN NOT MATCHED THEN INSERT VALUES (sid, delta, 'inserted'); }
step s2s { SELECT * FROM rm_t ORDER BY tid, val; }
step s2fe { EXPLAIN (COSTS OFF) MERGE INTO rm_t t USING rm_s s ON t.tid = s.sid WHEN MATCHED THEN UPDATE SET balance = balance + delta, val = val || ' updated' WHEN NOT MATCHED THEN INSERT VALUES (sid, delta, 'inserted') WHEN NOT MATCHED BY SOURCE THEN UPDATE SET val = val || ' by source'; }
step s2f { MERGE INTO rm_t t USING rm_s s ON t.tid = s.sid WHEN MATCHED THEN UPDATE SET balance = balance + delta, val = val || ' updated' WHEN NOT MATCHED THEN INSERT VALUES (sid, delta, 'inserted') WHEN NOT MATCHED BY SOURCE THEN UPDATE SET val = val || ' by source' RETURNING merge_action(), t.*; }
step s2fn { MERGE INTO rm_t t USING rm_s s ON t.tid = s.sid WHEN MATCHED THEN UPDATE SET balance = balance + delta, val = val || ' updated' WHEN NOT MATCHED THEN INSERT VALUES (sid, delta, 'inserted') WHEN NOT MATCHED BY SOURCE THEN UPDATE SET val = val || ' by source'; }
step s2fd { MERGE INTO rm_t t USING rm_s s ON t.tid = s.sid WHEN NOT MATCHED BY SOURCE THEN DELETE WHEN NOT MATCHED BY TARGET THEN INSERT VALUES (s.sid, s.delta) RETURNING merge_action(), t.*; }
step s2oe { EXPLAIN (COSTS OFF) MERGE INTO ro_t t USING ro_s s ON t.id = s.id WHEN MATCHED AND s.n <> t.n THEN UPDATE SET n = s.n, val = t.val || ' u' WHEN NOT MATCHED THEN INSERT VALUES (s.id + 100000, s.n, 'nm'); }
step s2om { MERGE INTO ro_t t USING ro_s s ON t.id = s.id WHEN MATCHED AND s.n <> t.n THEN UPDATE SET n = s.n, val = t.val || ' u' WHEN NOT MATCHED THEN INSERT VALUES (s.id + 100000, s.n, 'nm') RETURNING merge_action(), t.*; }
step s2ox { MERGE INTO ro_t t USING ro_s s ON t.id = s.id WHEN MATCHED AND s.n <> t.n THEN DELETE WHEN NOT MATCHED THEN INSERT VALUES (s.id + 100000, s.n, 'nm') RETURNING merge_action(), t.*; }
step s2oq { MERGE INTO ro_t t USING ro_s s ON t.id = s.id WHEN MATCHED AND s.n <> t.n THEN UPDATE SET n = s.n, val = t.val || ' c' || (SELECT count(*) FROM ro_c c WHERE c.id = t.id) WHEN NOT MATCHED AND s.id < 5000 THEN INSERT VALUES (s.id + 100000, s.n, 'nm') RETURNING merge_action(), t.*; }
step s2os { SELECT * FROM ro_t WHERE id <= 12 OR id > 4000 ORDER BY id, val; }

session s3
setup { SET enable_hashjoin = off; SET enable_nestloop = off; }
step s3e { EXPLAIN (COSTS OFF) MERGE INTO rm_t t USING rm_s s JOIN rm_s2 s2 ON sid = sid2 ON t.tid = sid WHEN MATCHED THEN UPDATE SET val = val || ' ' || sval WHEN NOT MATCHED THEN INSERT VALUES (sid, delta, sval); }
step s3m { MERGE INTO rm_t t USING rm_s s JOIN rm_s2 s2 ON sid = sid2 ON t.tid = sid WHEN MATCHED THEN UPDATE SET val = val || ' ' || sval WHEN NOT MATCHED THEN INSERT VALUES (sid, delta, sval); }


session s4
setup { SET enable_mergejoin = off; SET enable_nestloop = off; }
step s4e { EXPLAIN (COSTS OFF) MERGE INTO hf_t t USING hf_s s ON t.tid = s.sid WHEN MATCHED THEN UPDATE SET balance = balance + delta, val = val || ' updated' WHEN NOT MATCHED THEN INSERT VALUES (sid, delta, 'inserted') WHEN NOT MATCHED BY SOURCE AND (t.tid < 12 OR t.tid > 30000) THEN UPDATE SET val = val || ' by source'; }
step s4n { MERGE INTO hf_t t USING hf_s s ON t.tid = s.sid WHEN MATCHED THEN UPDATE SET balance = balance + delta, val = val || ' updated' WHEN NOT MATCHED THEN INSERT VALUES (sid, delta, 'inserted') WHEN NOT MATCHED BY SOURCE AND (t.tid < 12 OR t.tid > 30000) THEN UPDATE SET val = val || ' by source'; }
step s4r { MERGE INTO hf_t t USING hf_s s ON t.tid = s.sid WHEN MATCHED THEN UPDATE SET balance = balance + delta, val = val || ' updated' WHEN NOT MATCHED THEN INSERT VALUES (sid, delta, 'inserted') WHEN NOT MATCHED BY SOURCE AND (t.tid < 12 OR t.tid > 30000) THEN UPDATE SET val = val || ' by source' RETURNING merge_action(), t.tid, t.val; }
step s4s { SELECT * FROM hf_t WHERE tid < 12 OR tid > 30000 ORDER BY tid, val; }
permutation s2e s1n s2m s1c s2s
permutation s1k s2m s1c s2s
permutation s1d s2m s1c s2s
permutation s1n s2x s1c s2s
permutation s1k s2x s1c s2s
permutation s3e s1n s3m s1c s2s
permutation s1k s3m s1c s2s
permutation s1d s3m s1c s2s
permutation s2fe s1n s2f s1c s2s
permutation s1k s2fn s1c s2s
permutation s1d s2f s1c s2s
permutation s1u9 s2f s1c s2s
permutation s1d9 s2f s1c s2s
permutation s1u9 s2fd s1c s2s
permutation s1n s2fd s1c s2s
permutation s2oe s1on s2om s1c s2os
permutation s1ok s2om s1c s2os
permutation s1od s2om s1c s2os
permutation s1oq s2om s1c s2os
permutation s1on s2ox s1c s2os
permutation s1ok s2ox s1c s2os
permutation s1on s2oq s1c s2os
permutation s1ok s2oq s1c s2os
permutation s1od s2oq s1a s2os
permutation s4e s1hk s4n s1c s4s
permutation s1hk s4r s1c s4s

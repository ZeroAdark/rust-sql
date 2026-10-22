# mergecmd tw-core-197 (D1933 MEMO): a MERGE over a nested loop whose inner is a Memoize over the target's index scan (the
# source the outer, five keys repeated 40 times; hash and merge joins off), while another session updates, re-keys or
# deletes the target row the MERGE then waits on. PostgreSQL's EvalPlanQual builds a fresh Memoize (an empty cache), so
# the recheck runs the index scan over the target's newest version. The statement's cache already holds that key's old
# version when the MERGE waits: a recheck that read it would update from the old column values, keep a re-keyed or deleted
# row matched, or pass an action qual the newest version fails. s2u: MATCHED only (an INNER nest); s2n: with a NOT MATCHED
# INSERT (a LEFT nest: a row that no longer joins inserts its source row); s2q: the action qual reads the column s1 changes.
# s2s reads the table afterwards.

setup
{
  CREATE TABLE mq_t (id int, val int);
  CREATE INDEX mq_t_id ON mq_t (id);
  INSERT INTO mq_t SELECT g, g FROM generate_series(1, 5000) g;
  CREATE TABLE mq_s (id int, n int);
  INSERT INTO mq_s SELECT (g % 5) + 100, g / 5 FROM generate_series(0, 199) g;
  INSERT INTO mq_s VALUES (6001, 0), (6002, 0);
}
setup { ANALYZE mq_t; }
setup { ANALYZE mq_s; }

teardown
{
  DROP TABLE mq_t, mq_s;
}

session s1
setup { BEGIN; }
step s1u { UPDATE mq_t SET val = val + 1000 WHERE id = 102; }
step s1k { UPDATE mq_t SET id = 9102 WHERE id = 102; }
step s1d { DELETE FROM mq_t WHERE id = 102; }
step s1q { UPDATE mq_t SET val = -val WHERE id = 102; }
step s1c { COMMIT; }
step s1a { ROLLBACK; }

session s2
setup { SET enable_hashjoin = off; SET enable_mergejoin = off; }
step s2eu { EXPLAIN (COSTS OFF) MERGE INTO mq_t t USING mq_s s ON t.id = s.id WHEN MATCHED AND s.n = 0 THEN UPDATE SET val = t.val + 1; }
step s2en { EXPLAIN (COSTS OFF) MERGE INTO mq_t t USING mq_s s ON t.id = s.id WHEN MATCHED AND s.n = 0 THEN UPDATE SET val = t.val + 1 WHEN NOT MATCHED AND s.n <= 1 THEN INSERT VALUES (s.id, -1); }
step s2u { MERGE INTO mq_t t USING mq_s s ON t.id = s.id WHEN MATCHED AND s.n = 0 THEN UPDATE SET val = t.val + 1 RETURNING merge_action(), t.*; }
step s2n { MERGE INTO mq_t t USING mq_s s ON t.id = s.id WHEN MATCHED AND s.n = 0 THEN UPDATE SET val = t.val + 1 WHEN NOT MATCHED AND s.n <= 1 THEN INSERT VALUES (s.id, -1) RETURNING merge_action(), t.*; }
step s2q { MERGE INTO mq_t t USING mq_s s ON t.id = s.id WHEN MATCHED AND s.n = 0 AND t.val > 0 THEN UPDATE SET val = t.val + 1 RETURNING merge_action(), t.*; }
step s2s { SELECT id, val FROM mq_t WHERE id BETWEEN 100 AND 104 OR id > 5000 OR val < 0 ORDER BY id, val; }

permutation s2eu s2en s1u s2u s1c s2s
permutation s1k s2u s1c s2s
permutation s1d s2u s1c s2s
permutation s1u s2n s1c s2s
permutation s1k s2n s1c s2s
permutation s1d s2n s1c s2s
permutation s1q s2q s1c s2s
permutation s1u s2q s1c s2s
permutation s1k s2u s1a s2s

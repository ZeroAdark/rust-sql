# D3641: a write whose source is a flattened UNION ALL with bare relation members, hashed under the join with the target
# probing. When another session moves or changes the target row first, EvalPlanQual scans the members again and joins
# them with the newest version: an UPDATE takes its new values from that re-joined row, while a MERGE decides MATCHED by
# it and then runs its actions (their conditions too) over the ORIGINAL source row (ExecMergeMatched's planSlot): the
# key moved onto another member row, an action condition on the source, the key moved onto no member row (no action), a
# changed non-key column, and the UPDATE beside them.

setup
{
  CREATE TABLE ur_t (id int PRIMARY KEY, hits int NOT NULL DEFAULT 0);
  INSERT INTO ur_t SELECT i, 0 FROM generate_series(1, 2000) i WHERE i % 5 <> 0;
  CREATE TABLE ur_a (k int, w int);
  INSERT INTO ur_a VALUES (3, 1), (6, 2);
  CREATE TABLE ur_b (k int, w int);
  INSERT INTO ur_b VALUES (4, 101), (10, 103);
  ANALYZE ur_t, ur_a, ur_b;
}

teardown
{
  DROP TABLE ur_t, ur_a, ur_b;
}

session s1
setup { SET enable_nestloop = off; SET enable_mergejoin = off; }
step s1x { EXPLAIN (COSTS OFF) MERGE INTO ur_t t USING (SELECT k, w FROM ur_a UNION ALL SELECT k, w FROM ur_b) s ON t.id = s.k WHEN MATCHED THEN UPDATE SET hits = t.hits + s.w; }
step s1b { BEGIN; }
step s1m { MERGE INTO ur_t t USING (SELECT k, w FROM ur_a UNION ALL SELECT k, w FROM ur_b) s ON t.id = s.k WHEN MATCHED THEN UPDATE SET hits = t.hits + s.w; }
step s1a { MERGE INTO ur_t t USING (SELECT k, w FROM ur_a UNION ALL SELECT k, w FROM ur_b) s ON t.id = s.k WHEN MATCHED AND s.w < 50 THEN UPDATE SET hits = t.hits + s.w WHEN MATCHED THEN DELETE; }
step s1u { UPDATE ur_t t SET hits = t.hits + s.w FROM (SELECT k, w FROM ur_a UNION ALL SELECT k, w FROM ur_b) s WHERE t.id = s.k; }
step s1c { COMMIT; }
step s1r { SELECT id, hits FROM ur_t WHERE hits <> 0 OR id <= 16 ORDER BY id; }

session s2
step s2b { BEGIN; }
step s2k { UPDATE ur_t SET id = 10 WHERE id = 3; }
step s2g { UPDATE ur_t SET id = 15 WHERE id = 3; }
step s2h { UPDATE ur_t SET hits = 7 WHERE id = 3; }
step s2c { COMMIT; }

permutation s1x s2b s2k s1b s1m s2c s1c s1r
permutation s2b s2k s1b s1a s2c s1c s1r
permutation s2b s2g s1b s1m s2c s1c s1r
permutation s2b s2h s1b s1m s2c s1c s1r
permutation s2b s2h s1b s1a s2c s1c s1r
permutation s2b s2k s1b s1u s2c s1c s1r

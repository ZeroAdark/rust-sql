# mergecmd DO-NOTHING-JOIN: PostgreSQL 18.6 picks a MERGE's join from its actions with a command only, so a BY SOURCE DO
# NOTHING beside a MATCHED UPDATE and a NOT MATCHED INSERT (s2) plans the source LEFT JOIN the target (the target hashed),
# and an unqualified NOT MATCHED DO NOTHING beside a BY SOURCE DELETE (s3) plans the target LEFT JOIN the source. s1
# changes the target row the MERGE waits on: updated (n) it stays MATCHED (s2) or BY SOURCE (s3) at its newest version;
# re-keyed (k) the s2 row no longer joins and its source row is NOT MATCHED (inserted), while the s3 row stays BY SOURCE
# (deleted); deleted (d) the s2 source row inserts and the s3 row falls through to NOT MATCHED, DO NOTHING. The s2
# source is larger than the target (the target hashed), the s3 source smaller (the source hashed, the target the outer).

setup
{
  CREATE TABLE dj_t (id int, n int, v text);
  CREATE TABLE dj_s (id int, v text);
  CREATE TABLE dj_s2 (id int, v text);
  INSERT INTO dj_t SELECT i, i, 'old ' || i FROM generate_series(1, 400) i;
  INSERT INTO dj_s SELECT i * 3, 'src ' || i FROM generate_series(1, 2000) i;
  INSERT INTO dj_s2 SELECT i * 3, 'src ' || i FROM generate_series(1, 300) i;
}
setup { ANALYZE dj_t, dj_s, dj_s2; }

teardown
{
  DROP TABLE dj_t, dj_s, dj_s2;
}

session s1
setup { BEGIN; }
step s1n { UPDATE dj_t SET n = n + 1000 WHERE id IN (6, 395); }
step s1k { UPDATE dj_t SET id = id + 1 WHERE id IN (6, 395); }
step s1d { DELETE FROM dj_t WHERE id IN (6, 395); }
step s1c { COMMIT; }

session s2
step s2e { EXPLAIN (COSTS OFF) MERGE INTO dj_t t USING dj_s s ON t.id = s.id WHEN MATCHED THEN UPDATE SET n = t.n + 1, v = s.v WHEN NOT MATCHED BY SOURCE THEN DO NOTHING WHEN NOT MATCHED THEN INSERT VALUES (s.id, -1, s.v); }
step s2m { MERGE INTO dj_t t USING dj_s s ON t.id = s.id WHEN MATCHED THEN UPDATE SET n = t.n + 1, v = s.v WHEN NOT MATCHED BY SOURCE THEN DO NOTHING WHEN NOT MATCHED THEN INSERT VALUES (s.id, -1, s.v); }
step s2s { SELECT id, n, v FROM dj_t WHERE id IN (6, 7, 395, 396, 397) ORDER BY id, n; }
step s2c { SELECT count(*), sum(id), sum(n), count(*) FILTER (WHERE n = -1) AS inserted, count(*) FILTER (WHERE v LIKE 'src%') AS touched FROM dj_t; }

session s3
step s3e { EXPLAIN (COSTS OFF) MERGE INTO dj_t t USING dj_s2 s ON t.id = s.id WHEN MATCHED THEN UPDATE SET n = -t.n WHEN NOT MATCHED BY SOURCE AND t.id > 390 THEN DELETE WHEN NOT MATCHED THEN DO NOTHING; }
step s3m { MERGE INTO dj_t t USING dj_s2 s ON t.id = s.id WHEN MATCHED THEN UPDATE SET n = -t.n WHEN NOT MATCHED BY SOURCE AND t.id > 390 THEN DELETE WHEN NOT MATCHED THEN DO NOTHING; }

permutation s2e s1n s2m s1c s2s s2c
permutation s1k s2m s1c s2s s2c
permutation s1d s2m s1c s2s s2c
permutation s3e s1n s3m s1c s2s s2c
permutation s1k s3m s1c s2s s2c
permutation s1d s3m s1c s2s s2c

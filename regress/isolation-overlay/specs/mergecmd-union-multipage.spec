# mergecmd UNION-MULTIPAGE (main 14:25Z, from producerchain's H2 WRONG): a MERGE whose target rows come out of the result
# relations' union, each partition about 40 pages. s1 changes a row near the end of leaf 2 (page 30+) and the MERGE waits
# on it; at s1's commit the recheck reruns the feeds, the row's own one substituted with the newest version (merge/lower.rs
# `rel` stores every feed site's position empty, a zero block word included). Forms: s2 the settled union a nested loop's
# outer (G2(c)), s3 the arms streamed into a merge join's inner Sort (SORT-UNION), s4 the union under a hash build
# (TABLEOID-HASH), s5 the arms' Sort a MATCHED-only merge join's outer (SORT-UNION, the source's Sort the inner; v moves
# the matched rows across leaves). s1 updates a non-key column (n), moves the key off the join (k), moves the row to leaf 3 (m: PostgreSQL's
# concurrent-move error) or deletes it (d). A ctid read back as page 0 would write another row or none.

setup
{
  CREATE TABLE mp_t (k int, p int, n int, f text) PARTITION BY LIST (p);
  CREATE TABLE mp_t1 PARTITION OF mp_t FOR VALUES IN (1) WITH (autovacuum_enabled = off);
  CREATE TABLE mp_t2 (f text, x int, n int, p int, k int) WITH (autovacuum_enabled = off);
  ALTER TABLE mp_t2 DROP COLUMN x;
  ALTER TABLE mp_t ATTACH PARTITION mp_t2 FOR VALUES IN (2);
  CREATE TABLE mp_t3 PARTITION OF mp_t FOR VALUES IN (3) WITH (autovacuum_enabled = off);
  INSERT INTO mp_t SELECT k, 1 + (k - 1) / 1200, 0, repeat('x', 200) FROM generate_series(1, 3600) k;
  CREATE TABLE mp_s (k int, d int);
  INSERT INTO mp_s SELECT k, 1 FROM generate_series(1190, 3600, 10) k;
  ANALYZE mp_t, mp_s;
  CREATE FUNCTION mp_one() RETURNS int AS $$ BEGIN RETURN 1; END; $$ LANGUAGE plpgsql STABLE;
}

teardown
{
  DROP TABLE mp_t, mp_s;
  DROP FUNCTION mp_one();
}

session s1
setup { BEGIN; }
step s1n { UPDATE mp_t SET n = n + 100 WHERE k = 2390; }
step s1k { UPDATE mp_t SET k = k + 10000 WHERE k = 2390; }
step s1m { UPDATE mp_t SET p = 3 WHERE k = 2390; }
step s1d { DELETE FROM mp_t WHERE k = 2390; }
step s1c { COMMIT; }

session s2
setup { SET enable_hashjoin = off; SET enable_mergejoin = off; }
step s2m { MERGE INTO mp_t pt USING (SELECT mp_one() + 1 AS pid) AS q JOIN mp_t2 pt2 ON (q.pid = pt2.p) ON pt.k = pt2.k WHEN MATCHED THEN UPDATE SET n = pt.n + 1; }

session s3
setup { SET enable_hashjoin = off; SET enable_nestloop = off; }
step s3m { MERGE INTO mp_t t USING mp_s s ON t.k = s.k WHEN MATCHED THEN UPDATE SET n = t.n + 1 WHEN NOT MATCHED THEN INSERT VALUES (s.k, 1, 0, 'new'); }

session s4
setup { SET enable_mergejoin = off; SET enable_nestloop = off; }
step s4m { MERGE INTO mp_t t USING mp_s s ON t.k = s.k WHEN MATCHED THEN UPDATE SET n = t.n + 1; }
step s4s { SELECT tableoid::regclass, count(*), sum(n), sum(n * k) FROM mp_t GROUP BY 1 ORDER BY 1; }
step s4r { SELECT tableoid::regclass, k, p, n FROM mp_t WHERE k IN (2390, 12390, 2400) ORDER BY k; }

session s5
setup { SET enable_hashjoin = off; SET enable_nestloop = off; }
step s5e { EXPLAIN (COSTS OFF) MERGE INTO mp_t t USING mp_s s ON t.k = s.k WHEN MATCHED THEN UPDATE SET n = t.n + 1; }
step s5m { MERGE INTO mp_t t USING mp_s s ON t.k = s.k WHEN MATCHED THEN UPDATE SET n = t.n + 1; }
step s5v { MERGE INTO mp_t t USING mp_s s ON t.k = s.k WHEN MATCHED THEN UPDATE SET p = 1 + (t.p % 3), n = t.n + 1; }

permutation s1n s2m s1c s4s s4r
permutation s1k s2m s1c s4s s4r
permutation s1m s2m s1c s4s s4r
permutation s1d s2m s1c s4s s4r
permutation s1n s3m s1c s4s s4r
permutation s1k s3m s1c s4s s4r
permutation s1m s3m s1c s4s s4r
permutation s1d s3m s1c s4s s4r
permutation s1n s4m s1c s4s s4r
permutation s1k s4m s1c s4s s4r
permutation s1d s4m s1c s4s s4r
permutation s5e s1n s5m s1c s4s s4r
permutation s1k s5m s1c s4s s4r
permutation s1m s5m s1c s4s s4r
permutation s1d s5m s1c s4s s4r
permutation s1n s5v s1c s4s s4r
permutation s1d s5v s1c s4s s4r

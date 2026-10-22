# mergecmd D1933 nest forms under a concurrent writer of the target row the MERGE waits on. s2b: BITMAP-REF, the source a
# REFERENCE-marked bitmap heap scan of the target relation (held-out orm-0358's self join), the target's index scan the
# inner: the recheck refetches the source row at its junk ctid (the version the join read). s2o: the source another
# relation's BitmapOr heap scan, a LEFT nest (NOT MATCHED INSERT): a re-keyed target row no longer joins and its source row
# inserts. s2v: V-COPY, the target's own scan over a Materialize of a VALUES list: the recheck refills it from the stored
# VALUES row; a row that no longer joins takes the NOT MATCHED BY SOURCE action. s2w: a security barrier view's MERGE
# USING VALUES (updatable_views #810 / #814): a row the writer makes private fails the view's qual at the recheck.
# s3n / s3m: BY-SOURCE-NEST, NOT MATCHED BY SOURCE over a Nested Loop Left Join, the target's own scan the outer (an empty
# source: gen mg-0144; s3m with hash / merge joins and Materialize off over source rows): a null-extended row stays BY
# SOURCE at the target's newest version; a MATCHED row whose target is re-keyed comes null-extended (BY SOURCE).
# s2x: D1933 (b)'s form (b) with a VALUES list outer (its stored COPY row), the target under the Material: updated, the
# UPDATE runs over the newest version; re-keyed or deleted, the VALUES row takes the NOT MATCHED INSERT.
# s2a: ACTION-SUB, a SubPlan only the UPDATE action reads (correlated to the target row) beside the nest: the recheck
# evaluates it again over the newest version (s1 changes the column it reads); s3t: ACTION-CTE, the same over a
# MATERIALIZED CTE (regress with#239's form), the recheck reading the statement's CTE store; s4t: the same
# ACTION-CTE MERGE over PostgreSQL's Hash Right Join (the source hashed, the target's own scan probing: drain's spooled
# form, merge_drained_target), nest loops and merge joins off: updated, the action's SubPlan reads the newest version's
# column; re-keyed or deleted, the source row takes the NOT MATCHED INSERT (no RETURNING: the drained NOT MATCHED rows
# come in the hash table's order, any order by the multiset rule; s4s reads the table in order). s2j: D1684, NOT MATCHED BY SOURCE over a
# source a bitmap heap scan reads (held-out tw-core-149): a BY SOURCE row s1 updates stays BY SOURCE at its newest version
# (the source position empty: no row); a row s1 moves out of the action qual is left alone.

setup
{
  CREATE TABLE ib_pt (post_id bigint NOT NULL, tag_id int NOT NULL, note text, n int NOT NULL DEFAULT 0, PRIMARY KEY (post_id, tag_id));
  INSERT INTO ib_pt SELECT p, t, 'n' || p FROM (SELECT p, 1 + p % 40 AS t FROM generate_series(1, 10000) g(p) UNION SELECT p, 1 + (p * 7) % 40 FROM generate_series(1, 10000) g(p) UNION SELECT p, 1 + (p * 13) % 40 FROM generate_series(1, 10000) g(p) WHERE p % 3 = 0) s ORDER BY p, t;
  CREATE TABLE ib_s (k bigint, t int, note text);
  INSERT INTO ib_s SELECT i, CASE WHEN i % 2 = 0 THEN 1 + i % 40 ELSE i % 41 END, 's' || i FROM generate_series(1, 20000) i;
  CREATE INDEX ib_s_k ON ib_s (k);
  CREATE TABLE ib_t (k int, v text);
  INSERT INTO ib_t SELECT i, 'v' || i FROM generate_series(1, 30) i;
  CREATE TABLE ib_base (person text, visibility text);
  INSERT INTO ib_base VALUES ('Tom', 'public'), ('Dick', 'private'), ('Harry', 'public');
  CREATE VIEW ib_rw WITH (security_barrier = true) AS SELECT person FROM ib_base WHERE visibility = 'public';
  CREATE TABLE ib_one (id int PRIMARY KEY, n int NOT NULL, hits int NOT NULL DEFAULT 0);
  INSERT INTO ib_one VALUES (1, 666, 0), (2, 7, 0), (3, 8, 0);
  CREATE TABLE ib_empty (id int, n int);
  CREATE TABLE ib_src (id int, n int);
  CREATE TABLE ib_m (tid int, bal int, val text);
  INSERT INTO ib_m VALUES (1, 100, 'a'), (2, 200, 'b'), (3, 300, 'c');
  CREATE TABLE ib_a AS SELECT i AS k, i AS r, (i || ' v')::text v FROM generate_series(1, 16, 3) i;
  ALTER TABLE ib_a ADD UNIQUE (k);
  CREATE TABLE ib_h AS SELECT i AS k, i AS r, (i || ' v')::text v FROM generate_series(1, 400, 3) i;
  ALTER TABLE ib_h ADD UNIQUE (k);
  CREATE TABLE ib_c (a int, b text);
  INSERT INTO ib_c SELECT g, 'c' || g FROM generate_series(1, 40) g;
  CREATE TABLE ib_jobs (id int PRIMARY KEY, queue text NOT NULL, state text NOT NULL DEFAULT 'queued', n int NOT NULL DEFAULT 0);
  INSERT INTO ib_jobs SELECT g, (ARRAY['mail', 'sms', 'push', 'web'])[1 + g % 4], (ARRAY['queued', 'running', 'done', 'queued', 'failed'])[1 + g % 5] FROM generate_series(1, 4000) g;
  CREATE INDEX ib_jobs_qs ON ib_jobs (queue, state);
}
setup { VACUUM ANALYZE ib_pt; }
setup { VACUUM ANALYZE ib_s; }
setup { VACUUM ANALYZE ib_t; }
setup { VACUUM ANALYZE ib_one; }
setup { VACUUM ANALYZE ib_empty; }
setup { VACUUM ANALYZE ib_m; }
setup { VACUUM ANALYZE ib_jobs; }
setup { VACUUM ANALYZE ib_h; }
setup { INSERT INTO ib_src VALUES (2, 40), (9, 90); }

teardown
{
  DROP VIEW ib_rw;
  DROP TABLE ib_pt, ib_s, ib_t, ib_base, ib_one, ib_empty, ib_src, ib_m, ib_a, ib_h, ib_c, ib_jobs;
}

session s1
setup { BEGIN; }
step s1b { UPDATE ib_pt SET n = n + 1 WHERE post_id = 2005 AND tag_id = 36; }
step s1bk { UPDATE ib_pt SET tag_id = 37 WHERE post_id = 2005 AND tag_id = 36; }
step s1bd { DELETE FROM ib_pt WHERE post_id = 2005 AND tag_id = 36; }
step s1o { UPDATE ib_pt SET tag_id = 39 WHERE post_id = 4006 AND tag_id = 7; }
step s1v { UPDATE ib_t SET v = v || '!' WHERE k = 5; }
step s1vk { UPDATE ib_t SET k = 4 WHERE k = 5; }
step s1w { UPDATE ib_base SET visibility = 'private' WHERE person = 'Tom'; }
step s1n { UPDATE ib_one SET n = n + 1 WHERE id = 1; }
step s1mk { UPDATE ib_one SET id = 20 WHERE id = 2; }
step s1x { UPDATE ib_m SET bal = bal + 1 WHERE tid = 2; }
step s1xk { UPDATE ib_m SET tid = 50 WHERE tid = 2; }
step s1xd { DELETE FROM ib_m WHERE tid = 2; }
step s1a { UPDATE ib_a SET r = 20 WHERE k = 4; }
step s1ak { UPDATE ib_a SET k = 5 WHERE k = 4; }
step s1h { UPDATE ib_h SET r = 20 WHERE k = 4; }
step s1hk { UPDATE ib_h SET k = 9004 WHERE k = 4; }
step s1hd { DELETE FROM ib_h WHERE k = 7; }
step s1j { UPDATE ib_jobs SET n = n + 7 WHERE id = 3; }
step s1jq { UPDATE ib_jobs SET state = 'running' WHERE id = 3; }
step s1c { COMMIT; }

session s2
step s2b { MERGE INTO ib_pt pt USING (SELECT post_id, tag_id FROM ib_pt WHERE post_id BETWEEN 2000 AND 2010 AND tag_id > 30) s ON pt.post_id = s.post_id AND pt.tag_id = s.tag_id WHEN MATCHED THEN DELETE WHEN NOT MATCHED THEN DO NOTHING RETURNING merge_action(), pt.*; }
step s2bs { SELECT post_id, tag_id, n FROM ib_pt WHERE post_id IN (2005, 2010) ORDER BY 1, 2; }
step s2oe { EXPLAIN (COSTS OFF) MERGE INTO ib_pt pt USING (SELECT k, t, note FROM ib_s WHERE k BETWEEN 4000 AND 4012 OR k BETWEEN 7000 AND 7006) s ON pt.post_id = s.k AND pt.tag_id = s.t WHEN MATCHED THEN UPDATE SET note = s.note WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.t, s.note); }
step s2o { MERGE INTO ib_pt pt USING (SELECT k, t, note FROM ib_s WHERE k BETWEEN 4000 AND 4012 OR k BETWEEN 7000 AND 7006) s ON pt.post_id = s.k AND pt.tag_id = s.t WHEN MATCHED THEN UPDATE SET note = s.note WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.t, s.note); }
step s2os { SELECT post_id, tag_id, note FROM ib_pt WHERE post_id IN (4005, 4006) ORDER BY 1, 2; }
step s2v { MERGE INTO ib_t t USING (VALUES (3, 'a'), (7, 'b')) AS v(n, x) ON t.k < v.n AND t.k > v.n - 3 WHEN MATCHED THEN UPDATE SET v = t.v || v.x WHEN NOT MATCHED BY SOURCE AND t.k < 10 THEN DELETE RETURNING merge_action(), t.*; }
step s2vs { SELECT * FROM ib_t WHERE k < 12 ORDER BY 1, 2; }
step s2w { MERGE INTO ib_rw t USING (VALUES ('Tom'), ('Dick'), ('Harry')) AS v(person) ON t.person = v.person WHEN MATCHED THEN UPDATE SET person = v.person || '+'; }
step s2ws { SELECT * FROM ib_base ORDER BY 1; }
step s2xe { EXPLAIN (COSTS OFF) MERGE INTO ib_m t USING (VALUES (1, 5), (2, 6), (9, 7)) v(k, d) ON t.tid IS NOT DISTINCT FROM v.k WHEN MATCHED THEN UPDATE SET bal = t.bal + v.d WHEN NOT MATCHED THEN INSERT VALUES (v.k, v.d, 'v'); }
step s2x { MERGE INTO ib_m t USING (VALUES (1, 5), (2, 6), (9, 7)) v(k, d) ON t.tid IS NOT DISTINCT FROM v.k WHEN MATCHED THEN UPDATE SET bal = t.bal + v.d WHEN NOT MATCHED THEN INSERT VALUES (v.k, v.d, 'v') RETURNING merge_action(), t.*; }
step s2xs { SELECT * FROM ib_m ORDER BY tid, bal; }
step s2a { MERGE INTO ib_a m USING (SELECT g AS k, 'src ' || g AS v FROM generate_series(1, 8) g OFFSET 0) o ON m.k = o.k WHEN MATCHED THEN UPDATE SET v = (SELECT b FROM ib_c WHERE ib_c.a = m.r LIMIT 1) WHEN NOT MATCHED THEN INSERT VALUES (o.k, -1, o.v) RETURNING merge_action(), m.*; }
step s2ae { EXPLAIN (COSTS OFF) MERGE INTO ib_a m USING (SELECT g AS k, 'src ' || g AS v FROM generate_series(1, 8) g OFFSET 0) o ON m.k = o.k WHEN MATCHED THEN UPDATE SET v = (SELECT b FROM ib_c WHERE ib_c.a = m.r LIMIT 1) WHEN NOT MATCHED THEN INSERT VALUES (o.k, -1, o.v); }
step s2je { EXPLAIN (COSTS OFF) MERGE INTO ib_jobs j USING (SELECT id FROM ib_jobs WHERE state = 'queued' AND queue = 'mail') s ON j.id = s.id WHEN NOT MATCHED BY SOURCE AND j.state = 'queued' THEN UPDATE SET state = 'paused'; }
step s2j { MERGE INTO ib_jobs j USING (SELECT id FROM ib_jobs WHERE state = 'queued' AND queue = 'mail') s ON j.id = s.id WHEN NOT MATCHED BY SOURCE AND j.state = 'queued' THEN UPDATE SET state = 'paused'; }
step s2js { SELECT id, queue, state, n FROM ib_jobs WHERE id IN (3, 4, 8) ORDER BY id; }
step s2as { SELECT * FROM ib_a ORDER BY k; }

session s3
setup { SET enable_hashjoin = off; SET enable_mergejoin = off; SET enable_material = off; }
step s3e { EXPLAIN (COSTS OFF) MERGE INTO ib_one t USING ib_src s ON t.id = s.id WHEN MATCHED THEN UPDATE SET hits = hits + s.n WHEN NOT MATCHED BY SOURCE THEN UPDATE SET hits = -1; }
step s3n { MERGE INTO ib_one t USING ib_empty s ON t.id = s.id WHEN NOT MATCHED BY SOURCE AND t.id = 1 THEN UPDATE SET hits = hits + 5 RETURNING merge_action(), t.*; }
step s3m { MERGE INTO ib_one t USING ib_src s ON t.id = s.id WHEN MATCHED THEN UPDATE SET hits = hits + s.n WHEN NOT MATCHED BY SOURCE THEN UPDATE SET hits = -1 RETURNING merge_action(), t.*; }
step s3s { SELECT * FROM ib_one ORDER BY id; }
step s3t { WITH cte AS MATERIALIZED (SELECT g AS a, 'cte ' || g AS b FROM generate_series(1, 40) g) MERGE INTO ib_a m USING (SELECT g AS k, 'src ' || g AS v FROM generate_series(1, 8) g OFFSET 0) o ON m.k = o.k WHEN MATCHED THEN UPDATE SET v = (SELECT b FROM cte WHERE cte.a = m.r LIMIT 1) WHEN NOT MATCHED THEN INSERT VALUES (o.k, -1, o.v) RETURNING merge_action(), m.*; }


session s4
setup { SET enable_nestloop = off; SET enable_mergejoin = off; }
step s4te { EXPLAIN (COSTS OFF) WITH cte AS MATERIALIZED (SELECT g AS a, 'cte ' || g AS b FROM generate_series(1, 40) g) MERGE INTO ib_h m USING (SELECT g AS k, 'src ' || g AS v FROM generate_series(1, 8) g OFFSET 0) o ON m.k = o.k WHEN MATCHED THEN UPDATE SET v = (SELECT b FROM cte WHERE cte.a = m.r LIMIT 1) WHEN NOT MATCHED THEN INSERT VALUES (o.k, -1, o.v); }
step s4t { WITH cte AS MATERIALIZED (SELECT g AS a, 'cte ' || g AS b FROM generate_series(1, 40) g) MERGE INTO ib_h m USING (SELECT g AS k, 'src ' || g AS v FROM generate_series(1, 8) g OFFSET 0) o ON m.k = o.k WHEN MATCHED THEN UPDATE SET v = (SELECT b FROM cte WHERE cte.a = m.r LIMIT 1) WHEN NOT MATCHED THEN INSERT VALUES (o.k, -1, o.v); }
step s4s { SELECT * FROM ib_h WHERE k <= 10 OR k > 400 ORDER BY k; }
permutation s1b s2b s1c s2bs
permutation s1bk s2b s1c s2bs
permutation s1bd s2b s1c s2bs
permutation s2oe s1o s2o s1c s2os
permutation s1v s2v s1c s2vs
permutation s1vk s2v s1c s2vs
permutation s1w s2w s1c s2ws
permutation s3e s1n s3n s1c s3s
permutation s1n s3m s1c s3s
permutation s1mk s3m s1c s3s
permutation s2xe s1x s2x s1c s2xs
permutation s1xk s2x s1c s2xs
permutation s1xd s2x s1c s2xs
permutation s2ae s1a s2a s1c s2as
permutation s1ak s2a s1c s2as
permutation s2je s1j s2j s1c s2js
permutation s1jq s2j s1c s2js
permutation s1a s3t s1c s2as
permutation s1ak s3t s1c s2as
permutation s4te s1h s4t s1c s4s
permutation s1hk s4t s1c s4s
permutation s1hd s4t s1c s4s

# mergecmd D655 Row C (merge/epq.rs): a MERGE in a data-modifying CTE rechecks in the IR, as ExecMergeMatched's
# TM_Updated leg does. s2's CTE waits on target rows s1 changed; after s1 commits, the redo puts the newest version in
# the write site, reruns the join over it and runs the WHEN MATCHED list again from its first action. Three forms (hash
# and merge joins forced by nestloop / mergejoin off; EXPLAIN pins each plan): OWN (rmo, a one-row VALUES source folded
# into a qual of the target's own scan, no NOT MATCHED action), BUILT (rmt, a Hash Left Join whose build scans the target)
# and DRAIN (rmd, a Hash Right Join whose build scans the source, the target probing: the gen mg-0025 group's form). In
# each: another column changed keeps the row matched (its action over the newest version); the first action's WHEN
# condition flipped runs the first of two MATCHED actions instead of the second; a changed join key makes the row NOT
# MATCHED (ExecMergeMatched's NULL-ctid row: the NOT MATCHED INSERT of the source row; under OWN, with none, nothing; a
# MATCHED-only BUILT join: nothing); a deleted row runs the NOT MATCHED actions too (TM_Deleted). s2dg is the gen form
# (a MATCHED DELETE and a NOT MATCHED INSERT, RETURNING merge_action() and a target column). With s1 rolled back the
# MERGE writes the old versions. s2bs (a BY SOURCE action beside a MATCHED one) runs over a Hash Right Join whose build
# scans the target (BUILT-RIGHT): its row stays matched over the newest version (merge/epq.rs).
# D3379: DRAIN with a join qual past the hash clause (s2dj, `ON t.id = s.id AND t.n < s.n`) rechecks the qual over the
# newest version: another column changed keeps the row matched, n raised past the source's makes it NOT MATCHED (no
# INSERT: its WHEN NOT MATCHED condition fails), a re-keyed or deleted row is NOT MATCHED too.

setup
{
  CREATE TABLE rmo (k int PRIMARY KEY, v int NOT NULL);
  INSERT INTO rmo SELECT g, g * 10 FROM generate_series(1, 100) g;
  CREATE TABLE rmt (id int PRIMARY KEY, n int NOT NULL, tag text NOT NULL);
  INSERT INTO rmt SELECT g, g, 'old' FROM generate_series(1, 20) g;
  CREATE TABLE rms (id int NOT NULL, n int NOT NULL);
  INSERT INTO rms SELECT g, g * 100 FROM generate_series(1, 400) g;
  CREATE TABLE rmd (id int PRIMARY KEY, n int NOT NULL, tag text NOT NULL);
  INSERT INTO rmd SELECT g, g, 'old' FROM generate_series(1, 2000) g;
  CREATE TABLE rmq (id int NOT NULL, n int NOT NULL);
  INSERT INTO rmq VALUES (4, 40), (5, 50), (6, 60), (7, 70), (8, 80), (9, 90), (3001, 1), (3002, 2);
}
setup { ANALYZE rmo, rmt, rms, rmd, rmq; }

teardown
{
  DROP TABLE rmo, rmt, rms, rmd, rmq;
}

session s1
setup		{ BEGIN; }
step s1o	{ UPDATE rmo SET v = v + 5 WHERE k = 3; }
step s1ok	{ UPDATE rmo SET k = 1004 WHERE k = 4; }
step s1oq	{ UPDATE rmo SET v = 999 WHERE k = 3; }
step s1b	{ UPDATE rmt SET tag = 'new' WHERE id = 5; }
step s1bk	{ UPDATE rmt SET id = 1006 WHERE id = 6; }
step s1bd	{ DELETE FROM rmt WHERE id = 7; }
step s1bq	{ UPDATE rmt SET n = 1000 WHERE id = 8; }
step s1d	{ UPDATE rmd SET tag = 'new' WHERE id = 5; }
step s1dk	{ UPDATE rmd SET id = 5006 WHERE id = 6; }
step s1dd	{ DELETE FROM rmd WHERE id = 7; }
step s1dq	{ UPDATE rmd SET n = 1000 WHERE id = 8; }
step s1c	{ COMMIT; }
step s1r	{ ROLLBACK; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2xo	{ EXPLAIN (COSTS OFF) WITH m AS (MERGE INTO rmo t USING (VALUES (3, 1)) s(k, d) ON t.k = s.k WHEN MATCHED THEN UPDATE SET v = t.v + s.d RETURNING t.k, t.v) SELECT * FROM m; }
step s2xb	{ EXPLAIN (COSTS OFF) WITH m AS (MERGE INTO rmt t USING rms s ON t.id = s.id WHEN MATCHED AND t.n > 500 THEN DELETE WHEN MATCHED THEN UPDATE SET n = t.n + 1, tag = t.tag || '+' WHEN NOT MATCHED AND s.id <= 25 THEN INSERT VALUES (s.id, s.n, 'ins') RETURNING merge_action() AS act, t.id, t.n, t.tag) SELECT act, id, n, tag FROM m WHERE id BETWEEN 4 AND 9 OR id > 20 ORDER BY act, id; }
step s2xd	{ EXPLAIN (COSTS OFF) WITH m AS (MERGE INTO rmd t USING rmq s ON t.id = s.id WHEN MATCHED AND t.n > 500 THEN DELETE WHEN MATCHED THEN UPDATE SET n = t.n + 1, tag = t.tag || '+' WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.n, 'ins') RETURNING merge_action() AS act, t.id, t.n, t.tag) SELECT act, id, n, tag FROM m ORDER BY act, id; }
step s2o	{ WITH m AS (MERGE INTO rmo t USING (VALUES (3, 1)) s(k, d) ON t.k = s.k WHEN MATCHED THEN UPDATE SET v = t.v + s.d RETURNING t.k, t.v) SELECT * FROM m; }
step s2ok	{ WITH m AS (MERGE INTO rmo t USING (VALUES (4, 1)) s(k, d) ON t.k = s.k WHEN MATCHED THEN DELETE RETURNING t.k, t.v) SELECT * FROM m; }
step s2oq	{ WITH m AS (MERGE INTO rmo t USING (VALUES (3, 1)) s(k, d) ON t.k = s.k WHEN MATCHED AND t.v > 500 THEN DELETE WHEN MATCHED THEN UPDATE SET v = t.v + s.d RETURNING merge_action() AS act, t.k, t.v) SELECT * FROM m; }
step s2b	{ WITH m AS (MERGE INTO rmt t USING rms s ON t.id = s.id WHEN MATCHED AND t.n > 500 THEN DELETE WHEN MATCHED THEN UPDATE SET n = t.n + 1, tag = t.tag || '+' WHEN NOT MATCHED AND s.id <= 25 THEN INSERT VALUES (s.id, s.n, 'ins') RETURNING merge_action() AS act, t.id, t.n, t.tag) SELECT act, id, n, tag FROM m WHERE id BETWEEN 4 AND 9 OR id > 20 ORDER BY act, id; }
step s2bi	{ WITH m AS (MERGE INTO rmt t USING rms s ON t.id = s.id WHEN MATCHED THEN UPDATE SET n = t.n + 1, tag = t.tag || '+' RETURNING t.id, t.n, t.tag) SELECT id, n, tag FROM m WHERE id BETWEEN 4 AND 9 ORDER BY id; }
step s2d	{ WITH m AS (MERGE INTO rmd t USING rmq s ON t.id = s.id WHEN MATCHED AND t.n > 500 THEN DELETE WHEN MATCHED THEN UPDATE SET n = t.n + 1, tag = t.tag || '+' WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.n, 'ins') RETURNING merge_action() AS act, t.id, t.n, t.tag) SELECT act, id, n, tag FROM m ORDER BY act, id; }
step s2dg	{ WITH m AS (MERGE INTO rmd t USING rmq s ON t.id = s.id WHEN MATCHED THEN DELETE WHEN NOT MATCHED THEN INSERT (id, n, tag) VALUES (s.id, s.n, 'ins') RETURNING merge_action() AS act, t.n) SELECT act, count(*), sum(n) FROM m GROUP BY act ORDER BY act; }
step s2xdj	{ EXPLAIN (COSTS OFF) WITH m AS (MERGE INTO rmd t USING rmq s ON t.id = s.id AND t.n < s.n WHEN MATCHED THEN UPDATE SET n = t.n + 1, tag = t.tag || '+' WHEN NOT MATCHED AND s.id > 3000 THEN INSERT VALUES (s.id, s.n, 'ins') RETURNING merge_action() AS act, t.id, t.n, t.tag) SELECT act, id, n, tag FROM m ORDER BY act, id; }
step s2dj	{ WITH m AS (MERGE INTO rmd t USING rmq s ON t.id = s.id AND t.n < s.n WHEN MATCHED THEN UPDATE SET n = t.n + 1, tag = t.tag || '+' WHEN NOT MATCHED AND s.id > 3000 THEN INSERT VALUES (s.id, s.n, 'ins') RETURNING merge_action() AS act, t.id, t.n, t.tag) SELECT act, id, n, tag FROM m ORDER BY act, id; }
step s2bs	{ WITH m AS (MERGE INTO rmt t USING rms s ON t.id = s.id WHEN MATCHED THEN UPDATE SET n = t.n + 1 WHEN NOT MATCHED BY SOURCE THEN UPDATE SET tag = 'bs' RETURNING merge_action() AS act, t.id, t.n, t.tag) SELECT act, count(*), sum(n) FROM m GROUP BY act ORDER BY act; }

session s3
step s3o	{ SELECT k, v FROM rmo WHERE k IN (3, 4, 1004) ORDER BY k; }
step s3b	{ SELECT id, n, tag FROM rmt WHERE id BETWEEN 4 AND 9 OR id > 20 ORDER BY id; }
step s3d	{ SELECT id, n, tag FROM rmd WHERE id BETWEEN 4 AND 9 OR id > 2000 ORDER BY id; }

permutation s2xo s2xb s2xd s1o s2o s1c s3o
permutation s1ok s2ok s1c s3o
permutation s1oq s2oq s1c s3o
permutation s1b s1bk s1bd s1bq s2b s1c s3b
permutation s1b s1bk s2bi s1c s3b
permutation s1d s1dk s1dd s1dq s2d s1c s3d
permutation s1d s1dk s1dd s2dg s1c s3d
permutation s1b s1bk s1bq s2b s1r s3b
permutation s1b s2bs s1c s3b
permutation s2xdj s1d s1dk s1dd s1dq s2dj s1c s3d

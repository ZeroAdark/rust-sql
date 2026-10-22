# D3650 NEST-COPY: a data-modifying CTE's UPDATE over a Nested Loop whose outer is the target's own scan and whose inner
# is the CTE Scan of a materialized CTE (COPY-marked), the join key a Join Filter (graphile-worker's get_job `q` over
# one named queue), against a concurrent change of the target row (READ COMMITTED). The UPDATE finds the version its
# snapshot read updated and rechecks the newest one under EvalPlanQual, the CTE Scan handing on its stored row: a row
# changed but still joined is updated over the newest version, one whose key moved off the CTE's row or whose filter
# fails now is skipped, a deleted one is gone. The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE nc_q (id int PRIMARY KEY, locked_by text, n int NOT NULL DEFAULT 0);
  CREATE TABLE nc_src (id int PRIMARY KEY, qid int);
  INSERT INTO nc_q VALUES (1, NULL, 0);
  INSERT INTO nc_src VALUES (10, 1);
  ANALYZE nc_q; ANALYZE nc_src;
}
teardown { DROP TABLE nc_q, nc_src; }

session s1
setup		{ BEGIN; }
step s1bump	{ UPDATE nc_q SET n = n + 10 WHERE id = 1; }
step s1move	{ UPDATE nc_q SET id = 2 WHERE id = 1; }
step s1take	{ UPDATE nc_q SET locked_by = 'w1' WHERE id = 1; }
step s1del	{ DELETE FROM nc_q WHERE id = 1; }
step s1c	{ COMMIT; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) WITH j AS MATERIALIZED (SELECT id, qid FROM nc_src), q AS (UPDATE nc_q SET locked_by = 'w2', n = nc_q.n + 1 FROM j WHERE nc_q.id = j.qid AND nc_q.locked_by IS NULL RETURNING nc_q.id, nc_q.n, nc_q.locked_by) SELECT id, n, locked_by FROM q; }
step s2upd	{ WITH j AS MATERIALIZED (SELECT id, qid FROM nc_src), q AS (UPDATE nc_q SET locked_by = 'w2', n = nc_q.n + 1 FROM j WHERE nc_q.id = j.qid AND nc_q.locked_by IS NULL RETURNING nc_q.id, nc_q.n, nc_q.locked_by) SELECT id, n, locked_by FROM q; }
step s2show	{ SELECT id, locked_by, n FROM nc_q ORDER BY id; }

permutation s2x s1bump s2upd s1c s2show
permutation s1move s2upd s1c s2show
permutation s1take s2upd s1c s2show
permutation s1del s2upd s1c s2show

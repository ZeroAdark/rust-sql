# D3647 BUILT-COPY: a data-modifying CTE's UPDATE over a Hash Join whose build is the target's own scan (its filter
# locked_by IS NULL in that scan) and whose probe is the CTE Scan of a materialized CTE (COPY-marked): graphile-worker
# 0.17+ / 0.18.0's batched get_jobs `q` over fewer queues than the batch. Against a concurrent change of a target row
# (READ COMMITTED), the UPDATE finds the version its snapshot read updated and rechecks the newest one under
# EvalPlanQual: the CTE Scan hands on its stored row, the target's scan runs over the newest version and the hash
# clause and filter over the pair. A row changed but still matching is updated over the newest version (n keeps the
# concurrent + 10), one whose key moved off the CTE's rows or whose filter fails now is skipped, a deleted one is gone.
# Several CTE rows reach each target row; the later ones find it already updated by this command and skip it. The
# second form has no RETURNING. The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE bc_q (id int PRIMARY KEY, locked_by text, n int NOT NULL DEFAULT 0);
  CREATE TABLE bc_src (id int PRIMARY KEY, qid int);
  INSERT INTO bc_q VALUES (1, NULL, 0), (2, NULL, 0), (3, NULL, 0);
  INSERT INTO bc_src SELECT g, 1 + g % 3 FROM generate_series(1, 30) g;
  ANALYZE bc_q; ANALYZE bc_src;
}
teardown { DROP TABLE bc_q, bc_src; }

session s1
setup		{ BEGIN; }
step s1bump	{ UPDATE bc_q SET n = n + 10 WHERE id = 1; }
step s1move	{ UPDATE bc_q SET id = 4 WHERE id = 1; }
step s1take	{ UPDATE bc_q SET locked_by = 'w1' WHERE id = 1; }
step s1del	{ DELETE FROM bc_q WHERE id = 1; }
step s1c	{ COMMIT; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) WITH j AS MATERIALIZED (SELECT id, qid FROM bc_src), q AS (UPDATE bc_q SET locked_by = 'w2', n = bc_q.n + 1 FROM j WHERE bc_q.id = j.qid AND bc_q.locked_by IS NULL RETURNING bc_q.id, bc_q.n, bc_q.locked_by) SELECT DISTINCT id, n, locked_by FROM q ORDER BY id; }
step s2upd	{ WITH j AS MATERIALIZED (SELECT id, qid FROM bc_src), q AS (UPDATE bc_q SET locked_by = 'w2', n = bc_q.n + 1 FROM j WHERE bc_q.id = j.qid AND bc_q.locked_by IS NULL RETURNING bc_q.id, bc_q.n, bc_q.locked_by) SELECT DISTINCT id, n, locked_by FROM q ORDER BY id; }
step s2upd0	{ WITH j AS MATERIALIZED (SELECT id, qid FROM bc_src), q AS (UPDATE bc_q SET locked_by = 'w2', n = bc_q.n + 1 FROM j WHERE bc_q.id = j.qid AND bc_q.locked_by IS NULL) SELECT count(*) FROM j; }
step s2show	{ SELECT id, locked_by, n FROM bc_q ORDER BY id; }

permutation s2x s1bump s2upd s1c s2show
permutation s1move s2upd s1c s2show
permutation s1take s2upd s1c s2show
permutation s1del s2upd s1c s2show
permutation s1bump s2upd0 s1c s2show
permutation s1take s2upd0 s1c s2show

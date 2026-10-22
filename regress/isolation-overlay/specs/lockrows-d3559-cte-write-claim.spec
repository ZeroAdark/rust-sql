# D3559: a sorted claim in a CTE (Limit -> LockRows -> Sort -> scan) read by a data-modifying CTE's UPDATE .. FROM it,
# the statement reading that CTE's rows. Over a newer version of a claimed row (READ COMMITTED) PostgreSQL's
# EvalPlanQual reruns Sort -> scan over it in the claim: a row whose payload changed meanwhile is claimed with it, one
# no longer queued is skipped and the claim goes on to the next row; SKIP LOCKED skips a locked row. The UPDATE then
# writes the rows the claim locked. The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE cq (id int PRIMARY KEY, status text NOT NULL, payload text NOT NULL);
  INSERT INTO cq SELECT g, 'queued', 'p' || (100 + g) FROM generate_series(1, 20) g;
  ANALYZE cq;
}
teardown { DROP TABLE cq; }

session s1
setup		{ BEGIN; }
step s1u	{ UPDATE cq SET payload = 'p100' WHERE id = 1; }
step s1s	{ UPDATE cq SET status = 'done' WHERE id = 1; }
step s1k	{ SELECT id FROM cq WHERE id = 1 FOR UPDATE; }
step s1c	{ COMMIT; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) WITH c AS (SELECT id FROM cq WHERE status = 'queued' ORDER BY payload LIMIT 2 FOR UPDATE), u AS (UPDATE cq SET status = 'picked' FROM c WHERE cq.id = c.id RETURNING cq.*) SELECT id, status, payload FROM u ORDER BY id; }
step s2a	{ WITH c AS (SELECT id FROM cq WHERE status = 'queued' ORDER BY payload LIMIT 2 FOR UPDATE), u AS (UPDATE cq SET status = 'picked' FROM c WHERE cq.id = c.id RETURNING cq.*) SELECT id, status, payload FROM u ORDER BY id; }
step s2sk	{ WITH c AS (SELECT id FROM cq WHERE status = 'queued' ORDER BY payload LIMIT 2 FOR UPDATE SKIP LOCKED), u AS (UPDATE cq SET status = 'picked' FROM c WHERE cq.id = c.id RETURNING cq.*) SELECT id, status, payload FROM u ORDER BY id; }

permutation s2x s1u s2a s1c
permutation s1s s2a s1c
permutation s1k s2sk s1c

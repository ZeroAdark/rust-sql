# D3558: PgQueuer's dequeue: two sorted claims each in its own CTE (Limit -> LockRows -> Sort -> Seq Scan, FOR UPDATE), a
# DISTINCT over their union, and a data-modifying CTE's UPDATE of the claimed rows (its Hash Join reads the union's rows
# through the subquery's COPY mark), read by the statement. Each CTE's LockRows rechecks under its own EvalPlanQual (the
# Sort's subtree over the test tuple) and hands the row to its Limit; the UPDATE writes the rows the claims locked. A
# queued row s1 changed is rechecked: still queued (counted), picked (fails the queued claim's filter: skipped, the next
# row taken), deleted; a stale picked row whose heartbeat s1 refreshed fails the retry claim's filter; SKIP LOCKED skips a
# row s1 holds. Expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE dq (id int PRIMARY KEY, priority int NOT NULL, heartbeat timestamptz NOT NULL, status text NOT NULL, entrypoint text NOT NULL);
  INSERT INTO dq SELECT g, g % 3, CASE WHEN g % 7 = 0 THEN timestamptz '2026-01-01' + g * interval '1 second' ELSE now() END, CASE WHEN g % 7 = 0 THEN 'picked' ELSE 'queued' END, CASE WHEN g % 2 = 0 THEN 'fetch' ELSE 'mail' END FROM generate_series(1, 120) g;
  ANALYZE dq;
}
teardown { DROP TABLE dq; }

session s1
setup		{ BEGIN; }
step s1a	{ UPDATE dq SET priority = priority WHERE id = 2; }
step s1p	{ UPDATE dq SET status = 'picked', heartbeat = now() WHERE id = 2; }
step s1d	{ DELETE FROM dq WHERE id = 5; }
step s1h	{ UPDATE dq SET heartbeat = now() WHERE id = 112; }
step s1l	{ SELECT id FROM dq WHERE id = 2 FOR UPDATE; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) WITH a AS (SELECT id FROM dq WHERE status = 'queued' ORDER BY priority DESC, id LIMIT 3 FOR UPDATE), b AS (SELECT id FROM dq WHERE entrypoint = 'fetch' AND status = 'picked' AND heartbeat < now() - interval '30 seconds' ORDER BY heartbeat DESC, id LIMIT 2 FOR UPDATE), c AS (SELECT DISTINCT id FROM (SELECT id FROM a UNION ALL SELECT id FROM b) s), u AS (UPDATE dq SET status = 'picked', heartbeat = now() WHERE id = ANY (SELECT id FROM c) RETURNING *) SELECT id, priority, entrypoint FROM u ORDER BY priority DESC, id; }
step s2u	{ WITH a AS (SELECT id FROM dq WHERE status = 'queued' ORDER BY priority DESC, id LIMIT 3 FOR UPDATE), b AS (SELECT id FROM dq WHERE entrypoint = 'fetch' AND status = 'picked' AND heartbeat < now() - interval '30 seconds' ORDER BY heartbeat DESC, id LIMIT 2 FOR UPDATE), c AS (SELECT DISTINCT id FROM (SELECT id FROM a UNION ALL SELECT id FROM b) s), u AS (UPDATE dq SET status = 'picked', heartbeat = now() WHERE id = ANY (SELECT id FROM c) RETURNING *) SELECT id, priority, entrypoint FROM u ORDER BY priority DESC, id; }
step s2sk	{ WITH a AS (SELECT id FROM dq WHERE status = 'queued' ORDER BY priority DESC, id LIMIT 3 FOR UPDATE SKIP LOCKED), b AS (SELECT id FROM dq WHERE entrypoint = 'fetch' AND status = 'picked' AND heartbeat < now() - interval '30 seconds' ORDER BY heartbeat DESC, id LIMIT 2 FOR UPDATE SKIP LOCKED), c AS (SELECT DISTINCT id FROM (SELECT id FROM a UNION ALL SELECT id FROM b) s), u AS (UPDATE dq SET status = 'picked', heartbeat = now() WHERE id = ANY (SELECT id FROM c) RETURNING *) SELECT id, priority, entrypoint FROM u ORDER BY priority DESC, id; }

# queued, priority 2 first: ids 2, 5, 8, ..; the stale picked fetch rows by heartbeat DESC: 112, 98, ..
permutation s2x s1a s2u s1c
permutation s1p s2u s1c
permutation s1d s2u s1c
permutation s1h s2u s1c
permutation s1l s2sk s1c

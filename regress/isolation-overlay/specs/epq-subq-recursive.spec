# A WITH RECURSIVE cte joined under FOR UPDATE OF the table. The plan: a
# LockRows over a hash join whose probe side is the CTE Scan of the
# recursive union (the EXPLAIN step pins it). PG's LockRows pulls its
# subplan row by row and never stops it on its own; its EvalPlanQual
# recheck reads the CTE Scan's ROW_MARK_COPY row, never the union again,
# and rejoins it with the locked row's newest version. Served: decompose's
# reader walk over the union passes the LockRows (setop.rs `readers`).

setup
{
 CREATE TABLE docs (id int PRIMARY KEY, status text);
 INSERT INTO docs SELECT g, 'pending' FROM generate_series(1, 5) g;
 ANALYZE docs;
}

teardown
{
 DROP TABLE docs;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1u	{ UPDATE docs SET status = 'revoked' WHERE id = 3; }
step s1k	{ UPDATE docs SET id = 99 WHERE id = 3; }
step s1c	{ COMMIT; }

session s2
step expl	{ EXPLAIN (COSTS OFF) WITH RECURSIVE r AS (SELECT 1 AS id UNION ALL SELECT id + 1 FROM r WHERE id < 5) SELECT d.* FROM docs d JOIN r ON r.id = d.id FOR UPDATE OF d; }
step lock	{ WITH RECURSIVE r AS (SELECT 1 AS id UNION ALL SELECT id + 1 FROM r WHERE id < 5) SELECT d.* FROM docs d JOIN r ON r.id = d.id FOR UPDATE OF d; }

session s3
step sel	{ SELECT id, status FROM docs ORDER BY id; }

# Pass arm: locked row re-returned with its NEW status; the CTE feeds the
# recheck join from the shared-estate CTE state.
permutation expl s1u lock s1c sel
# Skip arm: the locked row's join key moves away; row skipped.
permutation expl s1k lock s1c sel

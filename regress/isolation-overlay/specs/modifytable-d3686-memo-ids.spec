# D3686: an UPDATE by an id list (pg-boss: `WHERE name = 'q1' AND id IN (SELECT UNNEST(..)) AND state = 'cancelled'`)
# over a table whose filter keeps few rows: PostgreSQL 18.6 plans a Nested Loop whose outer de-duplicates the list and
# whose inner is a Memoize over the target's index scan keyed by the id. Against a concurrent change of a listed row
# (READ COMMITTED), the UPDATE finds the version its snapshot read updated and rechecks the newest one under
# EvalPlanQual, whose fresh Memoize misses and runs the index scan over that version: a row bumped but still cancelled
# is written over its newest version, one resumed or moved off the list is skipped, a deleted one is gone. The same in
# a data-modifying CTE (pg-boss's `WITH results AS (UPDATE ..) SELECT ..`), by literal ids and under the generic plan
# (its HashAggregate outer). The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE skr (id uuid NOT NULL, name text NOT NULL, state text NOT NULL, retry_count int NOT NULL DEFAULT 0, PRIMARY KEY (name, id));
  INSERT INTO skr SELECT md5(g::text)::uuid, (ARRAY['q1', 'q2'])[g % 2 + 1], CASE WHEN g IN (1000, 2000) THEN 'cancelled' ELSE 'active' END, g % 3 FROM generate_series(1, 4000) g;
  ANALYZE skr;
}
teardown { DROP TABLE skr; }

session s1
setup		{ BEGIN; }
step s1bump	{ UPDATE skr SET retry_count = retry_count + 10 WHERE id = md5('1000')::uuid; }
step s1act	{ UPDATE skr SET state = 'active' WHERE id = md5('1000')::uuid; }
step s1move	{ UPDATE skr SET id = md5('9999')::uuid WHERE id = md5('1000')::uuid; }
step s1del	{ DELETE FROM skr WHERE id = md5('1000')::uuid; }
step s1c	{ COMMIT; }

session s2
setup		{ DEALLOCATE ALL; SET plan_cache_mode = force_generic_plan; PREPARE ck(text, uuid[]) AS WITH r AS (UPDATE skr SET state = 'created' WHERE name = $1 AND id IN (SELECT UNNEST($2::uuid[])) AND state = 'cancelled' RETURNING retry_count, state) SELECT retry_count, state FROM r; }
step s2x	{ EXPLAIN (COSTS OFF) UPDATE skr SET state = 'created' WHERE name = 'q1' AND id IN (SELECT UNNEST(ARRAY[md5('1000')::uuid, md5('2000')::uuid])) AND state = 'cancelled' RETURNING retry_count, state; }
step s2upd	{ UPDATE skr SET state = 'created' WHERE name = 'q1' AND id IN (SELECT UNNEST(ARRAY[md5('1000')::uuid, md5('2000')::uuid])) AND state = 'cancelled' RETURNING retry_count, state; }
step s2cx	{ EXPLAIN (COSTS OFF) EXECUTE ck('q1', ARRAY[md5('1000')::uuid, md5('2000')::uuid]); }
step s2cte	{ EXECUTE ck('q1', ARRAY[md5('1000')::uuid, md5('2000')::uuid]); }
step s2ctel	{ WITH r AS (UPDATE skr SET state = 'created' WHERE name = 'q1' AND id IN (SELECT UNNEST(ARRAY[md5('1000')::uuid, md5('2000')::uuid])) AND state = 'cancelled' RETURNING retry_count, state) SELECT retry_count, state FROM r; }
step s2show	{ SELECT state, retry_count FROM skr WHERE id IN (md5('1000')::uuid, md5('2000')::uuid, md5('9999')::uuid) ORDER BY retry_count, state; }

permutation s2x s1bump s2upd s1c s2show
permutation s1act s2upd s1c s2show
permutation s1move s2upd s1c s2show
permutation s1del s2upd s1c s2show
permutation s2cx s1bump s2cte s1c s2show
permutation s1act s2cte s1c s2show
permutation s1move s2ctel s1c s2show
permutation s1del s2cte s1c s2show
permutation s1bump s2ctel s1c s2show

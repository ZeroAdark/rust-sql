# D3619: pg-boss resumeJobs, an UPDATE by an id list over few rows (`WHERE name = 'q1' AND id IN (SELECT UNNEST(..))
# AND state = 'cancelled'`), at the top level and in a data-modifying CTE. PostgreSQL 18.6 plans a Nested Loop Semi Join
# whose outer is the target's Seq Scan and whose inner is a Materialize of the Subquery Scan over ProjectSet (the id
# list, COPY-marked). Against a concurrent change of a listed row (READ COMMITTED), the UPDATE finds the version its
# snapshot read updated and rechecks the newest one under EvalPlanQual, the Subquery Scan handing on its stored row: a
# row bumped but still cancelled is written over its newest version, one resumed or moved off the list by the other
# session is skipped, a deleted one is gone. The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE sjr (id uuid NOT NULL, name text NOT NULL, state text NOT NULL, retry_count int NOT NULL DEFAULT 0, PRIMARY KEY (name, id));
  INSERT INTO sjr SELECT md5(g::text)::uuid, (ARRAY['q1', 'q2'])[g % 2 + 1], CASE WHEN g % 50 = 0 THEN 'cancelled' ELSE 'active' END, g % 3 FROM generate_series(1, 400) g;
  ANALYZE sjr;
}
teardown { DROP TABLE sjr; }

session s1
setup		{ BEGIN; }
step s1bump	{ UPDATE sjr SET retry_count = retry_count + 10 WHERE id = md5('50')::uuid; }
step s1act	{ UPDATE sjr SET state = 'active' WHERE id = md5('50')::uuid; }
step s1move	{ UPDATE sjr SET id = md5('9999')::uuid WHERE id = md5('50')::uuid; }
step s1del	{ DELETE FROM sjr WHERE id = md5('50')::uuid; }
step s1c	{ COMMIT; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) UPDATE sjr SET state = 'created' WHERE name = 'q1' AND id IN (SELECT UNNEST(ARRAY[md5('50')::uuid, md5('100')::uuid])) AND state = 'cancelled' RETURNING retry_count, state; }
step s2upd	{ UPDATE sjr SET state = 'created' WHERE name = 'q1' AND id IN (SELECT UNNEST(ARRAY[md5('50')::uuid, md5('100')::uuid])) AND state = 'cancelled' RETURNING retry_count, state; }
step s2cte	{ WITH results AS (UPDATE sjr SET state = 'created' WHERE name = 'q1' AND id IN (SELECT UNNEST(ARRAY[md5('50')::uuid, md5('100')::uuid])) AND state = 'cancelled' RETURNING retry_count) SELECT COUNT(*), SUM(retry_count) FROM results; }
step s2show	{ SELECT state, retry_count FROM sjr WHERE id IN (md5('50')::uuid, md5('100')::uuid, md5('9999')::uuid) ORDER BY retry_count, state; }

permutation s2x s1bump s2upd s1c s2show
permutation s1act s2upd s1c s2show
permutation s1move s2upd s1c s2show
permutation s1del s2upd s1c s2show
permutation s1bump s2cte s1c s2show
permutation s1act s2cte s1c s2show

# W5 multilock Row A (lowerv4 write/lock_rerun.rs), review round 2 S2 (D2048's mechanism): a lock over an INNER hash
# join whose build producer holds a LEFT join (a REFERENCE mark on its nullable side, the rechecked row null-extended
# there) whose nullable side calls a VOLATILE function. PG's EPQ scans no row of a relation whose mark's ctid is NULL
# (EvalPlanQualFetchRowMark), so it never calls ntick there; the runner's rerun producer would scan the site's stale row.
# Refused by name (NULL_SIDE): the LEFT join inside the producer, FOR UPDATE OF a, b. Served (s2i): the INNER join
# inside the producer, rechecked. Expected output is the C oracle's (PostgreSQL 18.6, pgdg).

setup
{
  CREATE TABLE na (id int PRIMARY KEY, v int NOT NULL);
  INSERT INTO na SELECT i, i FROM generate_series(1, 2000) i;
  CREATE TABLE nb (id int PRIMARY KEY, a_id int NOT NULL, c_id int NOT NULL);
  INSERT INTO nb SELECT i, i, CASE WHEN i = 2 THEN 99 ELSE 1 END FROM generate_series(1, 6) i;
  CREATE TABLE nc (id int PRIMARY KEY, x int NOT NULL);
  INSERT INTO nc VALUES (1, 10), (3, 30), (4, 40);
  CREATE SEQUENCE ncnt;
  CREATE FUNCTION ntick(int) RETURNS bool LANGUAGE plpgsql VOLATILE AS $$ BEGIN PERFORM nextval('ncnt'); RETURN $1 >= 0; END $$;
  ANALYZE na, nb, nc;
}

teardown
{
  DROP TABLE na, nb, nc;
  DROP FUNCTION ntick(int);
  DROP SEQUENCE ncnt;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1a2	{ UPDATE na SET v = v + 1 WHERE id = 2; }
step s1a1	{ UPDATE na SET v = v + 1 WHERE id IN (1, 5); }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; SET join_collapse_limit = 1; }
step s2b	{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT a.id, b.id, c.id FROM na a JOIN (nb b LEFT JOIN nc c ON c.id = b.c_id AND ntick(c.x)) ON b.a_id = a.id FOR UPDATE OF a, b; }
step s2j	{ SELECT a.id, b.id, c.id FROM na a JOIN (nb b LEFT JOIN nc c ON c.id = b.c_id AND ntick(c.x)) ON b.a_id = a.id FOR UPDATE OF a, b; }
step s2xi	{ EXPLAIN (COSTS OFF) SELECT a.id, a.v, b.id, c.id FROM na a JOIN (nb b JOIN nc c ON c.id = b.c_id AND ntick(c.x)) ON b.a_id = a.id FOR UPDATE OF a, b; }
step s2i	{ SELECT a.id, a.v, b.id, c.id FROM na a JOIN (nb b JOIN nc c ON c.id = b.c_id AND ntick(c.x)) ON b.a_id = a.id FOR UPDATE OF a, b; }
step s2last	{ SELECT last_value, is_called FROM ncnt; }
step s2c	{ COMMIT; }

# the LEFT join inside the producer, both a and b locked: a2's row (b2 null-extended on nc) rechecked
permutation s2b s2x s1a2 s2j s1c s2last s2c
# the INNER join inside the producer (served): a1's and a5's rows rechecked, nc's row read at its ctid each time
permutation s2b s2xi s1a1 s2i s1c s2last s2c

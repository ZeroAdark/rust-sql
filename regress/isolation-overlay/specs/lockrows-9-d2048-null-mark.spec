# lockrows 9 (D2048): a row lock over an INNER hash join whose build producer holds a LEFT join (a REFERENCE mark on its
# nullable side qc; the rechecked row null-extended there) whose nullable side calls a VOLATILE function. PostgreSQL's
# EvalPlanQual fetches no row for a mark whose ctid is NULL (EvalPlanQualFetchRowMark: "non-locked rels could be on the
# inside of outer joins"), so its recheck never scans qc and never calls qtick there. marks.rs stored the empty row
# position for such a mark only when the rechecking pipeline itself held the LEFT join: here qc's site kept a stale
# position and the rerun build called qtick once more (nextval 4 where PG 18.6 says 3). Forms: FOR UPDATE OF a (one lock
# over the join, the D655 form), the nullable side scanned by its index (enable_seqscan off), and a one-row build.
# Expected output is the C oracle's (PostgreSQL 18, pgdg).
setup
{
  CREATE TABLE qa (id int PRIMARY KEY, v int NOT NULL);
  INSERT INTO qa SELECT i, i FROM generate_series(1, 2000) i;
  CREATE TABLE qb (id int PRIMARY KEY, a_id int NOT NULL, c_id int NOT NULL);
  INSERT INTO qb SELECT i, i, CASE WHEN i = 2 THEN 99 ELSE 1 END FROM generate_series(1, 300) i;
  CREATE TABLE qc (id int PRIMARY KEY, x int NOT NULL);
  INSERT INTO qc VALUES (1, 10), (3, 30), (4, 40);
  CREATE SEQUENCE qcnt;
  CREATE FUNCTION qtick(int) RETURNS bool LANGUAGE plpgsql VOLATILE AS $$ BEGIN PERFORM nextval('qcnt'); RETURN $1 >= 0; END $$;
  ANALYZE qa, qb, qc;
}

teardown
{
  DROP TABLE qa, qb, qc;
  DROP FUNCTION qtick(int);
  DROP SEQUENCE qcnt;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1a2	{ UPDATE qa SET v = v + 1 WHERE id = 2; }
step s1a1	{ UPDATE qa SET v = v + 1 WHERE id = 1; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; SET join_collapse_limit = 1; }
step s2b	{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT a.id, b.id, c.id FROM qa a JOIN (qb b LEFT JOIN qc c ON c.id = b.c_id AND qtick(c.x)) ON b.a_id = a.id FOR UPDATE OF a; }
step s2o	{ SELECT a.id, b.id, c.id FROM qa a JOIN (qb b LEFT JOIN qc c ON c.id = b.c_id AND qtick(c.x)) ON b.a_id = a.id FOR UPDATE OF a; }
step s2one	{ SELECT a.id, b.id, c.id FROM qa a JOIN (qb b LEFT JOIN qc c ON c.id = b.c_id AND qtick(c.x)) ON b.a_id = a.id WHERE b.id <= 2 FOR UPDATE OF a; }
step s2idx	{ SET enable_seqscan = off; }
step s2last	{ SELECT last_value, is_called FROM qcnt; }
step s2c	{ COMMIT; }

# no concurrency: the baseline call count
permutation s2b s2o s2last s2c
# a2's row (b2 null-extended on qc) rechecked: qc is scanned by no recheck
permutation s2b s2x s1a2 s2o s1c s2last s2c
# a1's row (b1 joined to qc row 1) rechecked: qc's row fetched by its ctid, qtick called once more as PG
permutation s2b s1a1 s2o s1c s2last s2c
# a two-row build: both rechecked
permutation s2b s1a1 s1a2 s2one s1c s2last s2c
# the nullable side by its index
permutation s2idx s2b s1a2 s2o s1c s2last s2c

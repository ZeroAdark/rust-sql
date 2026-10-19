-- Differential fixture for the 2026-09-13 audit bug batch 222
-- (backend/optimizer: rowcomparesel join-clause detection). Run via
-- scripts/bugs-batch222-e2e.sh against the frozen expected captured from
-- C PostgreSQL 18.6.
\set VERBOSITY verbose
\pset pager off

-- regression: a row comparison whose leading pair references an
-- outer-join-nulled Var, estimated at join level, counts base rels the way
-- NumRelids does (selfuncs.c:2265: root->outer_join_rels excluded).
CREATE TABLE rc_a (x int, y int);
CREATE TABLE rc_b (x int, y int);
CREATE TABLE rc_c (x int, y int);
INSERT INTO rc_a SELECT i, i FROM generate_series(1, 100) i;
INSERT INTO rc_b SELECT i, i FROM generate_series(1, 50) i;
INSERT INTO rc_c SELECT i, i FROM generate_series(1, 30) i;
ANALYZE rc_a;
ANALYZE rc_b;
ANALYZE rc_c;
SELECT count(*) FROM rc_a LEFT JOIN rc_b ON rc_a.x = rc_b.x
  JOIN rc_c ON (rc_b.x, rc_b.y) < (rc_c.x, rc_c.y);
SELECT count(*) FROM rc_a LEFT JOIN rc_b ON rc_a.x = rc_b.x
  JOIN rc_c ON rc_a.x = rc_c.x AND (rc_b.x, rc_b.y) < (10, 10);
EXPLAIN SELECT count(*) FROM rc_a LEFT JOIN rc_b ON rc_a.x = rc_b.x
  JOIN rc_c ON rc_a.x = rc_c.x AND (rc_b.x, rc_b.y) < (10, 10);
SELECT count(*) FROM rc_a LEFT JOIN rc_b ON rc_a.x = rc_b.x
  WHERE (rc_b.x, rc_b.y) < (10, 10) OR rc_b.x IS NULL;
DROP TABLE rc_a, rc_b, rc_c;

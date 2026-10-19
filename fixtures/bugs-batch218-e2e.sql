-- bugs/batch-218-mixed-reaudit: C-vs-pgrust parity for the matview /
-- execCurrent / cube fixes. Expected file captured from C 18.6
-- (scripts/regress-diff.sh --capture), except the ATPostAlterTypeCleanup
-- hunk, which is a deliberate divergence (hand-verified, see below).
\set VERBOSITY verbose

-- regression: an error inside refresh_by_match_merge's SPI
-- statements carries exactly one 'SQL statement "..."' CONTEXT line.
CREATE TABLE b218_t(a int, b text);
INSERT INTO b218_t VALUES (1, 'x');
CREATE MATERIALIZED VIEW b218_mv AS SELECT a, b FROM b218_t;
CREATE UNIQUE INDEX b218_mv_a ON b218_mv(a);
INSERT INTO b218_t VALUES (1, 'y');
DO $$
DECLARE ctx text;
BEGIN
  REFRESH MATERIALIZED VIEW CONCURRENTLY b218_mv;
EXCEPTION WHEN unique_violation THEN
  GET STACKED DIAGNOSTICS ctx = PG_EXCEPTION_CONTEXT;
  RAISE NOTICE 'context: %', regexp_replace(ctx, 'pg_temp_\d+', 'pg_temp_N', 'g');
END $$;
INSERT INTO b218_t VALUES (1, 'x');
DO $$
DECLARE ctx text;
BEGIN
  REFRESH MATERIALIZED VIEW CONCURRENTLY b218_mv;
EXCEPTION WHEN cardinality_violation THEN
  GET STACKED DIAGNOSTICS ctx = PG_EXCEPTION_CONTEXT;
  RAISE NOTICE 'context: %', regexp_replace(ctx, 'pg_temp_\d+', 'pg_temp_N', 'g');
END $$;
DROP MATERIALIZED VIEW b218_mv;
DROP TABLE b218_t;

-- regression: a FOR UPDATE cursor over a partitioned table
-- whose partitions are all pruned at plan time still has es_rowmarks, so
-- WHERE CURRENT OF takes the rowmark arm's error text.
CREATE TABLE b218_pt(a int) PARTITION BY LIST (a);
CREATE TABLE b218_pt1 PARTITION OF b218_pt FOR VALUES IN (1);
CREATE TABLE b218_u(a int);
BEGIN;
DECLARE b218_c CURSOR FOR SELECT * FROM b218_pt WHERE a = 2 FOR UPDATE;
UPDATE b218_u SET a = 1 WHERE CURRENT OF b218_c;
ROLLBACK;
DROP TABLE b218_pt;
DROP TABLE b218_u;

-- regression: g_cube_union over a duplicated single entry keeps
-- the leaf key verbatim (cube_union_v0 a == b shortcut), so the downlink
-- built for the lone non-NULL entry at a NULL split stays unnormalized.
CREATE EXTENSION cube;
CREATE EXTENSION pageinspect;
CREATE TABLE b218_cube(c cube);
CREATE INDEX b218_cube_idx ON b218_cube USING gist(c);
INSERT INTO b218_cube VALUES ('(2,1),(1,2)');
INSERT INTO b218_cube SELECT NULL FROM generate_series(1, 600);
SELECT keys FROM gist_page_items(get_raw_page('b218_cube_idx', 0), 'b218_cube_idx') WHERE keys IS NOT NULL;
DROP TABLE b218_cube;

-- regression: cube_union detoasts before the a == b shortcut,
-- so a toasted (compressed in-line) cube passed twice is really unioned.
ALTER TYPE cube SET (STORAGE = extended);
CREATE TABLE b218_tc(c cube, pad bytea);
INSERT INTO b218_tc SELECT cube(array_prepend(2.0, array_fill(0.0, ARRAY[99])), array_prepend(1.0, array_fill(0.0, ARRAY[99]))), repeat('x', 1200)::bytea;
SELECT cube_ll_coord(cube_union(c, c), 1), cube_ur_coord(cube_union(c, c), 1), cube_union(c, c)::text LIKE '(1,%' AS normalized, cube_union(c, c) = c AS same FROM b218_tc;
SELECT cube_dim(cube_union(c, c)), cube_union(c, c) @> c AS contains, cube_ll_coord(c, 1), cube_ur_coord(c, 1) FROM b218_tc;
DROP TABLE b218_tc;

-- regression (pg-also-bug, deliberate divergence): C 18.6
-- reruns ATPostAlterTypeCleanup after AT_PASS_SET_EXPRESSION over the lists
-- the ALTER TYPE pass already consumed and fails with XX000 "cache lookup
-- failed for index <oid>"; pgrust consumes the lists once and the statement
-- succeeds. Expected hunk hand-verified.
CREATE TABLE b218_se (a int, b int, g int GENERATED ALWAYS AS (b * 2) STORED);
CREATE INDEX b218_se_a_idx ON b218_se (a);
ALTER TABLE b218_se ALTER COLUMN a TYPE bigint, ALTER COLUMN g SET EXPRESSION AS (b * 3);
SELECT pg_get_indexdef('b218_se_a_idx'::regclass);
SELECT attname, format_type(atttypid, atttypmod), pg_get_expr(adbin, adrelid) FROM pg_attribute a JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum WHERE a.attrelid = 'b218_se'::regclass;
DROP TABLE b218_se;

-- Differential fixture for the 2026-09-13 audit bug batch 255
-- (backend/utils/sort: a unique-index sort stops calling the user comparator
-- at the first duplicate pair, as C's comparetup_index_btree ereports from
-- inside the comparator). Run via scripts/bugs-batch255-e2e.sh against the
-- frozen expected captured from C PostgreSQL 18.6.
\pset pager off
-- regression: the NOTICE trail of a side-effecting btree
-- comparator is the observable; pgrust used to finish the whole qsort after
-- recording the violation (348 compares vs C's 198 on this input).
CREATE FUNCTION noisy_cmp(a int, b int) RETURNS int LANGUAGE plpgsql IMMUTABLE AS $$
BEGIN RAISE NOTICE 'cmp % %', a, b; RETURN btint4cmp(a, b); END $$;
CREATE OPERATOR CLASS noisy_ops FOR TYPE int USING btree AS
  OPERATOR 1 <, OPERATOR 2 <=, OPERATOR 3 =, OPERATOR 4 >=, OPERATOR 5 >,
  FUNCTION 1 noisy_cmp(int, int);
CREATE TABLE b255_dup AS SELECT (g * 7919) % 997 AS g FROM generate_series(1, 60) g;
INSERT INTO b255_dup SELECT g FROM b255_dup ORDER BY g LIMIT 1;
CREATE UNIQUE INDEX b255_dup_u ON b255_dup (g noisy_ops);
-- The non-unique build runs the same qsort to completion on both engines.
CREATE INDEX b255_dup_i ON b255_dup (g noisy_ops);
SELECT count(*) FROM b255_dup;
DROP TABLE b255_dup;
DROP OPERATOR CLASS noisy_ops USING btree;
DROP FUNCTION noisy_cmp(int, int);

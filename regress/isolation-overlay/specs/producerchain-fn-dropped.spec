# D3374 (lowerv4 rerun/epq/copy_row.rs, decompose function_record_row): an UPDATE / DELETE whose COPY-marked FROM row is a
# plpgsql function's row of a type with a dropped attribute, the target on the hash build side, meets a target row s1
# changed (READ COMMITTED). EvalPlanQual hands the function row back from its stored copy (the dropped attribute NULL) and
# reruns the build over the target's newest version: a version that still joins is written over its new values, one whose
# key moved off the joined row is skipped, a deleted one is skipped.
# The e2e twin is e2e/gravity/v5/producerchain_d3374_fn_dropped.sql.

setup
{
 CREATE TABLE dt (id int PRIMARY KEY, k int NOT NULL, x int NOT NULL);
 INSERT INTO dt SELECT i, i, 0 FROM generate_series(1, 1500) i;
 CREATE TABLE ds (k int, v text);
 INSERT INTO ds SELECT i, 'v' || i FROM generate_series(1, 4000) i;
 CREATE TYPE dty AS (k int, zz int, v text);
 ALTER TYPE dty DROP ATTRIBUTE zz;
 CREATE FUNCTION dp() RETURNS SETOF dty LANGUAGE plpgsql STABLE ROWS 4000 AS $$ BEGIN RETURN QUERY SELECT k, v FROM ds; END $$;
 ANALYZE dt, ds;
}

teardown
{
 DROP TABLE dt;
 DROP FUNCTION dp();
 DROP TABLE ds;
 DROP TYPE dty;
}

session s1
step b1 { BEGIN; }
step xup { UPDATE dt SET x = x + 1 WHERE id = 3; }
step kmv { UPDATE dt SET k = k + 1000 WHERE id = 3; }
step del { DELETE FROM dt WHERE id = 3; }
step c1 { COMMIT; }

session s2
step u2 { UPDATE dt SET x = x + 10 FROM dp() d WHERE d.k = dt.k AND d.k < 5 RETURNING dt.id, dt.k, dt.x, d; }
step d2 { DELETE FROM dt USING dp() d WHERE d.k = dt.k AND d.k < 5 RETURNING dt.id, dt.x, d; }
step sel { SELECT id, k, x FROM dt WHERE id < 6 OR k > 1000 ORDER BY id; }

permutation b1 xup u2 c1 sel
permutation b1 kmv u2 c1 sel
permutation b1 del u2 c1 sel
permutation b1 xup d2 c1 sel
permutation b1 kmv d2 c1 sel

# mergecmd VALUES-INNER (updatable_views 822): a MERGE / UPDATE whose target's own scan is the outer of an INNER nested
# loop over a COPY-marked VALUES scan (a MERGE and an UPDATE into a security barrier view over another).
# s2's write waits on s1's row lock; at s1's commit its recheck substitutes the newest version and re-opens the VALUES
# inner over the stored COPY row alone, as PostgreSQL's EvalPlanQual hands the Values Scan its stored row: a key s1 moves
# to another VALUES row's key no longer joins (k), a row s1 hides from the barrier qual (p) or deletes (d) is skipped,
# a non-key change (n) is written.

setup
{
  CREATE TABLE vi_base (person text, visibility text, n int);
  INSERT INTO vi_base VALUES ('Tom', 'public', 0), ('Dick', 'private', 0), ('Harry', 'public', 0);
  CREATE VIEW vi_view1 WITH (security_barrier = true) AS SELECT person, n FROM vi_base WHERE visibility = 'public';
  CREATE FUNCTION vi_true(text) RETURNS boolean LANGUAGE plpgsql COST 0.000001 AS $$ BEGIN RETURN true; END $$;
  CREATE VIEW vi_view2 WITH (security_barrier = true) AS SELECT * FROM vi_view1 WHERE vi_true(person);
}

teardown
{
  DROP VIEW vi_view2;
  DROP VIEW vi_view1;
  DROP TABLE vi_base;
  DROP FUNCTION vi_true(text);
}

session s1
setup { BEGIN; }
step s1n { UPDATE vi_base SET n = n + 1 WHERE person = 'Tom'; }
step s1p { UPDATE vi_base SET visibility = 'private' WHERE person = 'Tom'; }
step s1k { UPDATE vi_base SET person = 'Harry' WHERE person = 'Tom'; }
step s1d { DELETE FROM vi_base WHERE person = 'Tom'; }
step s1c { COMMIT; }

session s2
step s2m { MERGE INTO vi_view2 t USING (VALUES ('Tom'), ('Dick'), ('Harry')) AS v(person) ON t.person = v.person WHEN MATCHED THEN UPDATE SET n = t.n + 10; }
step s2u { UPDATE vi_view2 t SET n = t.n + 100 FROM (VALUES ('Tom'), ('Harry')) AS v(person) WHERE t.person = v.person; }
step s2e { EXPLAIN (COSTS OFF) MERGE INTO vi_view2 t USING (VALUES ('Tom'), ('Dick'), ('Harry')) AS v(person) ON t.person = v.person WHEN MATCHED THEN UPDATE SET n = t.n + 10; }
step s2f { EXPLAIN (COSTS OFF) UPDATE vi_view2 t SET n = t.n + 100 FROM (VALUES ('Tom'), ('Harry')) AS v(person) WHERE t.person = v.person; }
step s2s { SELECT person, visibility, n FROM vi_base ORDER BY person, n; }

permutation s2e s2f s1c
permutation s1n s2m s1c s2s
permutation s1p s2m s1c s2s
permutation s1k s2m s1c s2s
permutation s1d s2m s1c s2s
permutation s1k s2u s1c s2s
permutation s1n s2u s1c s2s

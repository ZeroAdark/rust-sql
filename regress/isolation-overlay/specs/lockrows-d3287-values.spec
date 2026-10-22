# lockrows D3287 (b): a row lock over an INNER nested loop whose inner is a VALUES list (FOR UPDATE OF the outer
# relation; the VALUES list's mark is ROW_MARK_COPY, its whole row carried as junk). EvalPlanQual reruns the join over
# the locked row's newest version and the one VALUES row the statement joined it to (EvalPlanQualFetchRowMark's copy
# leg), never the whole list. The nested loop (its outer the locked relation's scan) where the concurrent update keeps
# the join key (the match stays), moves it away (the recheck finds no match), onto another key the list holds (the
# stored row is still the one read: no match), and out of the WHERE clause; one locked row joined to two VALUES rows
# (each output row rechecks with its own stored row); plus the multi-statement form (the receiver stops at the
# recheck's push, D3289) and a cursor. (With enable_material on PG plans the VALUES list outer over a Materialize of the
# locked relation: another D3287 form, refused by name.)
# Expected output is the C oracle's (PostgreSQL 18, pgdg).
setup
{
  CREATE TABLE vo (id int PRIMARY KEY, k int, v text);
  INSERT INTO vo SELECT g, g, 'v' || g FROM generate_series(1, 40) g;
  ANALYZE vo;
}

teardown
{
  DROP TABLE vo;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step keep	{ UPDATE vo SET v = 'new' WHERE id = 2; }
step away	{ UPDATE vo SET k = 9 WHERE id = 2; }
step onto	{ UPDATE vo SET k = 3 WHERE id = 2; }
step out	{ UPDATE vo SET id = 100 WHERE id = 2; }
step c1		{ COMMIT; }

session s2
setup
{
  BEGIN ISOLATION LEVEL READ COMMITTED;
  SET LOCAL enable_hashjoin = off;
  SET LOCAL enable_mergejoin = off;
  SET LOCAL enable_material = off;
}
step nplan	{ EXPLAIN (COSTS OFF) SELECT vo.id, vo.k, vo.v, vs.tag FROM vo JOIN (VALUES (1, 'a'), (2, 'b'), (2, 'bb'), (3, 'c'), (5, 'e'), (6, 'f'), (7, 'g'), (8, 'h')) AS vs(k, tag) ON vs.k = vo.k WHERE vo.id <= 3 FOR UPDATE OF vo; }
step nlock	{ SELECT vo.id, vo.k, vo.v, vs.tag FROM vo JOIN (VALUES (1, 'a'), (2, 'b'), (2, 'bb'), (3, 'c'), (5, 'e'), (6, 'f'), (7, 'g'), (8, 'h')) AS vs(k, tag) ON vs.k = vo.k WHERE vo.id <= 3 FOR UPDATE OF vo; }
step nmulti	{
  SELECT 1;
  SELECT vo.id, vo.k, vo.v, vs.tag FROM vo JOIN (VALUES (1, 'a'), (2, 'b'), (2, 'bb'), (3, 'c'), (5, 'e'), (6, 'f'), (7, 'g'), (8, 'h')) AS vs(k, tag) ON vs.k = vo.k WHERE vo.id <= 3 FOR UPDATE OF vo;
}
step cur	{ DECLARE c CURSOR FOR SELECT vo.id, vo.k, vo.v, vs.tag FROM vo JOIN (VALUES (1, 'a'), (2, 'b'), (2, 'bb'), (3, 'c'), (5, 'e'), (6, 'f'), (7, 'g'), (8, 'h')) AS vs(k, tag) ON vs.k = vo.k WHERE vo.id <= 3 FOR UPDATE OF vo; }
step fetch1	{ FETCH 1 FROM c; }
step fetchall	{ FETCH ALL FROM c; }
step c2		{ COMMIT; }

permutation nplan keep nlock c1 c2
permutation nplan away nlock c1 c2
permutation nplan onto nlock c1 c2
permutation nplan out nlock c1 c2
permutation nplan away nmulti c1 c2
permutation nplan keep nmulti c1 c2
permutation cur fetch1 away fetchall c1 c2
permutation cur fetch1 keep fetchall c1 c2

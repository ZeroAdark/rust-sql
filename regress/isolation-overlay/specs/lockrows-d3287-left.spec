# lockrows D3287 (b): a row lock over a LEFT join (FOR UPDATE OF the outer relation; the nullable side is a REFERENCE
# relation). EvalPlanQual reruns the join over the locked row's newest version and the inner row the statement read,
# refetched by its ctid; a row the join null-extended carries that ctid NULL, so the recheck finds no inner row and
# null-extends again. Forms: a hash LEFT join (the inner hashed), a LEFT nested loop and one over a Materialize of the inner
# (m7_epq_joins' plan: the recheck's fresh Material pulls the refetched row), each where the concurrent update
# keeps the join key (the match stays), moves it away (the recheck null-extends) and onto another inner row (the inner row
# read stays the one refetched: no match), and a row that was null-extended; plus the multi-statement form (the receiver
# stops at the recheck's push, D3289) and a cursor.
# Expected output is the C oracle's (PostgreSQL 18, pgdg).
setup
{
  CREATE TABLE lo (id int PRIMARY KEY, k int, v text);
  CREATE TABLE li (k int, label text);
  INSERT INTO lo SELECT g, g, 'v' || g FROM generate_series(1, 8) g;
  INSERT INTO li VALUES (1, 'one'), (2, 'two'), (3, 'three'), (5, 'five'), (6, 'six');
  ANALYZE lo;
  ANALYZE li;
}

teardown
{
  DROP TABLE lo;
  DROP TABLE li;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step keep	{ UPDATE lo SET v = 'new' WHERE id = 2; }
step away	{ UPDATE lo SET k = 4 WHERE id = 2; }
step onto	{ UPDATE lo SET k = 3 WHERE id = 2; }
step nulled	{ UPDATE lo SET v = 'nul' WHERE id = 4; }
step c1		{ COMMIT; }

session s2
setup
{
  BEGIN ISOLATION LEVEL READ COMMITTED;
  SET LOCAL enable_mergejoin = off;
  SET LOCAL enable_nestloop = off;
}
step hplan	{ EXPLAIN (COSTS OFF) SELECT lo.id, lo.k, lo.v, li.label FROM lo LEFT JOIN li ON li.k = lo.k WHERE lo.id <= 6 FOR UPDATE OF lo; }
step hlock	{ SELECT lo.id, lo.k, lo.v, li.label FROM lo LEFT JOIN li ON li.k = lo.k WHERE lo.id <= 6 FOR UPDATE OF lo; }
step hmulti	{
  SELECT 1;
  SELECT lo.id, lo.k, lo.v, li.label FROM lo LEFT JOIN li ON li.k = lo.k WHERE lo.id <= 6 FOR UPDATE OF lo;
}
step nplan	{
  SET LOCAL enable_hashjoin = off;
  SET LOCAL enable_nestloop = on;
  SET LOCAL enable_material = off;
  EXPLAIN (COSTS OFF) SELECT lo.id, lo.k, lo.v, li.label FROM lo LEFT JOIN li ON li.k = lo.k WHERE lo.id <= 6 FOR UPDATE OF lo;
}
step nlock	{ SELECT lo.id, lo.k, lo.v, li.label FROM lo LEFT JOIN li ON li.k = lo.k WHERE lo.id <= 6 FOR UPDATE OF lo; }
step nmulti	{
  SELECT 1;
  SELECT lo.id, lo.k, lo.v, li.label FROM lo LEFT JOIN li ON li.k = lo.k WHERE lo.id <= 6 FOR UPDATE OF lo;
}
step mplan	{
  SET LOCAL enable_hashjoin = off;
  SET LOCAL enable_nestloop = on;
  EXPLAIN (COSTS OFF) SELECT lo.id, lo.k, lo.v, li.label FROM lo LEFT JOIN li ON li.k = lo.k WHERE lo.id <= 6 FOR UPDATE OF lo;
}
step mlock	{ SELECT lo.id, lo.k, lo.v, li.label FROM lo LEFT JOIN li ON li.k = lo.k WHERE lo.id <= 6 FOR UPDATE OF lo; }
step mmulti	{
  SELECT 1;
  SELECT lo.id, lo.k, lo.v, li.label FROM lo LEFT JOIN li ON li.k = lo.k WHERE lo.id <= 6 FOR UPDATE OF lo;
}
step cur	{ DECLARE c CURSOR FOR SELECT lo.id, lo.k, lo.v, li.label FROM lo LEFT JOIN li ON li.k = lo.k WHERE lo.id <= 6 FOR UPDATE OF lo; }
step fetch1	{ FETCH 1 FROM c; }
step fetchall	{ FETCH ALL FROM c; }
step c2		{ COMMIT; }

permutation hplan keep hlock c1 c2
permutation away hlock c1 c2
permutation onto hlock c1 c2
permutation nulled hlock c1 c2
permutation away hmulti c1 c2
permutation nplan keep nlock c1 c2
permutation nplan away nlock c1 c2
permutation nplan onto nlock c1 c2
permutation nplan nulled nlock c1 c2
permutation nplan away nmulti c1 c2
permutation mplan keep mlock c1 c2
permutation mplan away mlock c1 c2
permutation mplan onto mlock c1 c2
permutation mplan nulled mlock c1 c2
permutation mplan away mmulti c1 c2
permutation cur fetch1 away fetchall c1 c2

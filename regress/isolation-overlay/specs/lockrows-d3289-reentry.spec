# lockrows D3289: a row lock's recheck behind a receiver's stop at its push. The lock pushes the rows its out block holds
# before it stops for the recheck; when the receiver stops the call at that push (the last statement of a multi-statement
# simple query), the call leaves with the cursor AT the row and the resumed call takes the row again. A join step below
# the lock must hand that same row on again rather than continue past it: PostgreSQL returns the locked row's newest
# version. Forms: an INNER nested loop with both relations locked and one, each as the last statement of a multi-statement
# query and alone (control); a hash join over unique keys (control) and over a build with duplicate keys (a match chain);
# an INNER merge join (D3269 (a)), both locked and one; a cursor's FETCH.
# Expected output is the C oracle's (PostgreSQL 18, pgdg).
setup
{
  CREATE TABLE nj AS SELECT generate_series(1, 10) AS id, 0 AS data;
  CREATE INDEX ON nj (id);
  CREATE TABLE nk AS SELECT generate_series(1, 10) AS id, 0 AS data;
  CREATE INDEX ON nk (id);
  CREATE TABLE nd AS SELECT g AS id, s AS seq FROM generate_series(6, 7) g, generate_series(1, 2) s;
  ANALYZE nj;
  ANALYZE nk;
  ANALYZE nd;
}

teardown
{
  DROP TABLE nj;
  DROP TABLE nk;
  DROP TABLE nd;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step uj		{ UPDATE nj SET data = 42 WHERE id = 7; }
step c1		{ COMMIT; }

session s2
setup
{
  BEGIN ISOLATION LEVEL READ COMMITTED;
  SET LOCAL enable_hashjoin = off;
  SET LOCAL enable_mergejoin = off;
  SET LOCAL enable_material = off;
}
step plan	{ EXPLAIN (COSTS OFF) SELECT * FROM nj a JOIN nk b ON a.id = b.id FOR UPDATE; }
step both	{
  SELECT 1;
  SELECT * FROM nj a JOIN nk b ON a.id = b.id FOR UPDATE;
}
step one	{
  SELECT 1;
  SELECT * FROM nj a JOIN nk b ON a.id = b.id FOR UPDATE OF a;
}
step alone	{ SELECT * FROM nj a JOIN nk b ON a.id = b.id FOR UPDATE; }
step hash	{
  SET LOCAL enable_hashjoin = on;
  SET LOCAL enable_nestloop = off;
  SELECT * FROM nj a JOIN nk b ON a.id = b.id FOR UPDATE;
}
step hplan	{
  SET LOCAL enable_hashjoin = on;
  SET LOCAL enable_nestloop = off;
  EXPLAIN (COSTS OFF) SELECT * FROM nj a JOIN nd d ON a.id = d.id FOR UPDATE OF a;
}
step chain	{
  SELECT 1;
  SELECT * FROM nj a JOIN nd d ON a.id = d.id ORDER BY a.id, d.seq;
  SELECT * FROM nj a JOIN nd d ON a.id = d.id FOR UPDATE OF a;
}
step mplan	{
  SET LOCAL enable_mergejoin = on;
  SET LOCAL enable_nestloop = off;
  EXPLAIN (COSTS OFF) SELECT * FROM nj a JOIN nk b ON a.id = b.id FOR UPDATE;
}
step mboth	{
  SELECT 1;
  SELECT * FROM nj a JOIN nk b ON a.id = b.id FOR UPDATE;
}
step mone	{
  SELECT 1;
  SELECT * FROM nj a JOIN nk b ON a.id = b.id FOR UPDATE OF a;
}
step cur	{ DECLARE c CURSOR FOR SELECT * FROM nj a JOIN nk b ON a.id = b.id FOR UPDATE OF a; }
step fetch6	{ FETCH 6 FROM c; }
step fetchall	{ FETCH ALL FROM c; }
step c2		{ COMMIT; }

permutation plan uj both c1 c2
permutation uj one c1 c2
permutation uj alone c1 c2
permutation uj hash c1 c2
permutation hplan uj chain c1 c2
permutation mplan uj mboth c1 c2
permutation mplan uj mone c1 c2
permutation cur fetch6 uj fetchall c1 c2
permutation uj cur fetchall c1 c2

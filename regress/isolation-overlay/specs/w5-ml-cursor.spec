# W5 multilock Row A (lowerv4 write/lock_marks.rs): a cursor over a self-join that locks both references (the shape of
# regress portals' "multiple FOR UPDATE/SHARE references", over a plain table and a hash join here). FETCH locks the
# rows it returns: s1's update of either row of s2's current row waits until s2 ends. WHERE CURRENT OF raises "has
# multiple FOR UPDATE/SHARE references to table" (execCurrentOf's es_rowmarks). With FOR SHARE OF a, WHERE CURRENT OF
# updates a's row, and s1's update of b's row does not wait.
# Expected output is the C oracle's (PostgreSQL 18, pgdg).

setup
{
  CREATE TABLE mt (f1 int PRIMARY KEY, f2 text);
  INSERT INTO mt SELECT i, 'r' || i FROM generate_series(1, 10) i;
  ANALYZE mt;
}

teardown
{
  DROP TABLE mt;
}

session s1
setup		{ BEGIN; }
step s1ua	{ UPDATE mt SET f2 = 's1' WHERE f1 = 6; }
step s1ub	{ UPDATE mt SET f2 = 's1' WHERE f1 = 1; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2b	{ BEGIN; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT * FROM mt a, mt b WHERE a.f1 = b.f1 + 5 FOR UPDATE; }
step s2d	{ DECLARE c CURSOR FOR SELECT * FROM mt a, mt b WHERE a.f1 = b.f1 + 5 FOR UPDATE; }
step s2ds	{ DECLARE c CURSOR FOR SELECT * FROM mt a, mt b WHERE a.f1 = b.f1 + 5 FOR SHARE OF a; }
step s2f	{ FETCH 1 FROM c; }
step s2u	{ UPDATE mt SET f2 = 'cur' WHERE CURRENT OF c; }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT f1, f2 FROM mt WHERE f1 IN (1, 6) ORDER BY f1; }

# both references locked: s1's update of a's row waits; WHERE CURRENT OF raises (s2's transaction aborted: s1 goes on)
permutation s2b s2x s2d s2f s1ua s2u s2c s1c s3sel
permutation s2b s2d s2f s1ub s2c s1c s3sel
# FOR SHARE OF a: WHERE CURRENT OF updates a's row; b's row is free for s1
permutation s2b s2ds s2f s2u s1ub s1c s2c s3sel

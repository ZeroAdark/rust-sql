# writesubplan A2: an UPDATE / DELETE through a view whose DO INSTEAD rule writes the base table WHERE a = OLD.a, OLD
# being a view over that table with OFFSET 0 (a subquery the planner keeps): PostgreSQL 18.6 plans a Nested Loop whose
# outer is the target's own index scan (a = 3) and whose inner is the Subquery Scan of the view over a bitmap heap scan,
# the subquery's row marked ROW_MARK_COPY (its whole row carried as junk). READ COMMITTED: s2's write waits on s1's row
# lock; after s1 commits, EvalPlanQual reruns the plan with the target's newest version and hands the Subquery Scan the
# stored row (never the subquery re-run), its quals checked again over it. So the SET and RETURNING values the rule takes
# from OLD come from the row s2 first read (s1's change to b is overwritten), a target row whose key moved or that was
# deleted is skipped, and with s2 first s1 waits on it. s2m (several keys) plans the Subquery Scan as the outer and the
# target's index scan keyed by its row as the inner: the recheck hands the outer its stored row the same way.

setup
{
 CREATE TABLE wnc (a int PRIMARY KEY, b text DEFAULT 'Unspecified');
 INSERT INTO wnc SELECT i, 'Row ' || i FROM generate_series(-2, 6) g(i);
 CREATE VIEW wnc_v1 AS SELECT * FROM wnc WHERE a > 0 OFFSET 0;
 CREATE VIEW wnc_v2 AS SELECT * FROM wnc_v1 WHERE a < 10;
 CREATE RULE wnc_v1_upd AS ON UPDATE TO wnc_v1 DO INSTEAD UPDATE wnc SET b = NEW.b WHERE a = OLD.a RETURNING NEW.*;
 CREATE RULE wnc_v1_del AS ON DELETE TO wnc_v1 DO INSTEAD DELETE FROM wnc WHERE a = OLD.a RETURNING OLD.*;
}

teardown
{
 DROP TABLE wnc CASCADE;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1b	{ UPDATE wnc SET b = b || ' s1' WHERE a = 3; }
step s1k	{ UPDATE wnc SET a = 30 WHERE a = 4; }
step s1d	{ DELETE FROM wnc WHERE a = 2; }
step s1o	{ UPDATE wnc SET b = 'other' WHERE a = 5; }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2b	{ UPDATE wnc_v2 SET b = b || '!' WHERE a = 3 RETURNING old.*, new.*; }
step s2k	{ UPDATE wnc_v2 SET b = b || '!' WHERE a = 4 RETURNING *; }
step s2d	{ DELETE FROM wnc_v2 WHERE a = 2 RETURNING old.*, new.*; }
step s2bd	{ DELETE FROM wnc_v2 WHERE a = 3 RETURNING *; }
step s2m	{ UPDATE wnc_v2 SET b = b || '!' WHERE a IN (2, 3, 4) RETURNING *; }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT a, b FROM wnc ORDER BY a; }
step s3pu	{ EXPLAIN (COSTS OFF) UPDATE wnc_v2 SET b = b || '!' WHERE a = 3; }
step s3pd	{ EXPLAIN (COSTS OFF) DELETE FROM wnc_v2 WHERE a = 2; }

permutation s3pu s3pd s1c s2c
permutation s1b s1o s2b s1c s2c s3sel
permutation s1k s2k s1c s2c s3sel
permutation s1d s2d s1c s2c s3sel
permutation s1b s2bd s1c s2c s3sel
permutation s1b s1k s2m s1c s2c s3sel
permutation s2b s1b s2c s1c s3sel

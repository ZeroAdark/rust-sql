# writesubplan (D1531 x D1112): INSERT ... ON CONFLICT DO UPDATE SET (b, c) = (SELECT ..), a correlated MULTIEXPR SubPlan over
# the existing row, meets rows s1 changed or deleted. READ COMMITTED: s2's arm locks the conflicting row and waits on s1;
# after s1 commits, ExecOnConflictUpdate's lock sees the row updated and the insert restarts from the pre-check, so the
# arm (the MULTIEXPR, then the SET entries reading its Params) runs over the newest version (row 2's c + 1000, b 'u');
# row 3, which s1 deleted, no longer conflicts and is inserted. With s2 first, s1 waits and updates s2's row.

setup
{
 CREATE TABLE wc_t (a int PRIMARY KEY, b text, c int);
 INSERT INTO wc_t SELECT i, 'b' || i, i * 10 FROM generate_series(1, 4) i;
 CREATE TABLE wc_s (a int PRIMARY KEY, g int NOT NULL, v int NOT NULL);
 INSERT INTO wc_s SELECT i, i % 3, i * 7 FROM generate_series(1, 12) i;
 ANALYZE wc_t, wc_s;
}

teardown
{
 DROP TABLE wc_t;
 DROP TABLE wc_s;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1u	{ UPDATE wc_t SET c = c + 1000, b = 'u' WHERE a = 2; }
step s1d	{ DELETE FROM wc_t WHERE a = 3; }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2i	{ INSERT INTO wc_t VALUES (1, 'n'), (2, 'n'), (3, 'n') ON CONFLICT (a) DO UPDATE SET (b, c) = (SELECT wc_t.b || '+' || count(*), max(s.v) + wc_t.c FROM wc_s s WHERE s.g = wc_t.c % 3); }
step s2x	{ INSERT INTO wc_t AS t VALUES (2, 'x'), (4, 'x') ON CONFLICT (a) DO UPDATE SET (c, b) = (SELECT t.c + 1, t.b || excluded.b) WHERE t.c < 2000; }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT a, b, c FROM wc_t ORDER BY a; }

permutation s1u s1d s2i s1c s2c s3sel
permutation s1u s2x s1c s2c s3sel
permutation s2i s1u s2c s1c s3sel
permutation s2x s1u s2c s1c s3sel

# D1933B nest (modifytable, M3): an UPDATE / DELETE joined to a VALUES list, which PostgreSQL plans as a nested loop
# over the VALUES scan with the target's index scan inner, meets a target row s1 changed. EvalPlanQual hands the VALUES
# row back from its copy (ROW_MARK_COPY, never evaluated again) and reruns the inner over the newest version alone: a
# version that still passes the inner's filter and the join qual is written (SET over the new values), one that no
# longer passes (its flag cleared, its key moved) or was deleted is skipped. The same over a small table scanned as the
# outer (D1933 (a), its REFERENCE mark): the recheck refetches that table's row at its ctid, the version the statement
# joined, so s1's change to it is not seen.

setup
{
 CREATE TABLE nk (id int PRIMARY KEY, flag bool NOT NULL, val int);
 INSERT INTO nk SELECT i, true, i * 10 FROM generate_series(1, 20000) i;
 CREATE TABLE ne (id int PRIMARY KEY, val int);
 INSERT INTO ne VALUES (5, 50), (7, 70), (9, 90), (11, 110), (13, 130);
 ANALYZE nk, ne;
}

teardown
{
 DROP TABLE nk;
 DROP TABLE ne;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE nk SET val = val + 1000 WHERE id = 5; }
step s1f	{ UPDATE nk SET flag = false WHERE id = 7; }
step s1k	{ UPDATE nk SET id = 30009 WHERE id = 9; }
step s1d	{ DELETE FROM nk WHERE id = 11; }
step s1e	{ UPDATE ne SET val = val + 5000 WHERE id IN (5, 13); }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2u	{ UPDATE nk t SET val = t.val + v.x FROM (VALUES (5, 1), (7, 2), (9, 3), (11, 4), (13, 5)) v(k, x) WHERE t.id = v.k AND t.flag RETURNING t.id, t.val; }
step s2d	{ DELETE FROM nk t USING (VALUES (5), (7), (9), (11), (13)) v(k) WHERE t.id = v.k AND t.flag RETURNING t.id, t.val; }
step s2r	{ UPDATE nk t SET val = t.val + e.val FROM ne e WHERE e.id = t.id AND t.flag RETURNING t.id, t.val; }
step s2q	{ DELETE FROM nk t USING ne e WHERE e.id = t.id AND t.flag AND e.val < 1000 RETURNING t.id; }
step s2c	{ COMMIT; }

session s3
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; }
step s3sel	{ SELECT id, flag, val FROM nk WHERE id IN (5, 7, 9, 11, 13, 30009) ORDER BY id; }
step s3e	{ SELECT id, val FROM ne ORDER BY id; }
step s3pu	{ EXPLAIN (COSTS OFF) UPDATE nk t SET val = t.val + v.x FROM (VALUES (5, 1), (7, 2), (9, 3), (11, 4), (13, 5)) v(k, x) WHERE t.id = v.k AND t.flag; }
step s3pr	{ EXPLAIN (COSTS OFF) UPDATE nk t SET val = t.val + e.val FROM ne e WHERE e.id = t.id AND t.flag; }

permutation s3pu s3pr s1c s2c
permutation s1v s1f s1k s1d s2u s1c s2c s3sel
permutation s1v s1f s1k s1d s2d s1c s2c s3sel
permutation s2u s1v s2c s1c s3sel
permutation s1v s1f s1k s1d s1e s2r s1c s2c s3sel s3e
permutation s1v s1e s2q s1c s2c s3sel s3e

# producerchain D: an UPDATE / DELETE whose target's own scan is the outer of a chain of nested loops over Materials of
# other relations, with an INNER hash join in the chain whose build is another relation's own scan (subselect#98: a view
# rule's UPDATE over `FROM a JOIN b ON a.k = (SELECT .. WHERE c.k = b.k)`, the build hashed by a SubPlan key; or a hash
# join on one target column below a nested loop over a Material on another, the merge join off), meets a target row s1
# changed. EvalPlanQual reruns the plan with the target's newest version and every other relation's row refetched at its
# junk ctid, the version the statement joined: the hash join hashes that build row again (its SubPlan key over the same
# snapshot) and probes it with the newest version's key. A version whose hash key moved off the joined build row is
# skipped though its nested loop filter still passes, one whose nested loop key moved or that was deleted is skipped, one
# that still joins is written (SET over its new values, the rule's NEW columns from the joined rows as the statement read
# them), and s1's changes to the other relations are not seen.

setup
{
 CREATE TABLE pn_i4 (f1 int4);
 INSERT INTO pn_i4 VALUES (0), (123456), (-123456), (2147483647), (-2147483647);
 CREATE TABLE pn_t (ttype char(2), ordnum int4, partnum text, value float8);
 INSERT INTO pn_t VALUES ('wt', 0, '1', 1.5), ('wt', 123456, '2', 2.5), ('wt', -2147483647, '3', 3.5), ('xx', -123456, '4', 4.5), ('wt', 5, '5', 5.5);
 CREATE VIEW pn_v AS SELECT * FROM pn_t WHERE ttype = 'wt';
 CREATE RULE pn_v_upd AS ON UPDATE TO pn_v DO INSTEAD
   UPDATE pn_t SET partnum = new.partnum, value = new.value WHERE ttype = new.ttype AND ordnum = new.ordnum;
 CREATE TABLE pn_u (ordnum int4, k2 int4, n int NOT NULL DEFAULT 0);
 INSERT INTO pn_u VALUES (0, 0, 0), (123456, 123456, 0), (-2147483647, 2147483647, 0), (-123456, -123456, 0), (5, 0, 0);
}
setup { VACUUM pn_i4; }

teardown
{
 DROP VIEW pn_v;
 DROP TABLE pn_t;
 DROP TABLE pn_u;
 DROP TABLE pn_i4;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1p	{ UPDATE pn_t SET partnum = 'p' || partnum WHERE ordnum IN (0, -2147483647); }
step s1k	{ UPDATE pn_t SET ordnum = 7 WHERE ordnum = 123456; }
step s1t	{ UPDATE pn_t SET ttype = 'xx' WHERE ordnum = 0; }
step s1n	{ UPDATE pn_u SET n = n + 100 WHERE ordnum = 0; }
step s1m	{ UPDATE pn_u SET ordnum = 7 WHERE ordnum = 123456; }
step s1j	{ UPDATE pn_u SET k2 = -123456 WHERE ordnum = -2147483647; }
step s1d	{ DELETE FROM pn_u WHERE ordnum = -123456; }
step s1b	{ UPDATE pn_i4 SET f1 = 1 WHERE f1 = 123456; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_mergejoin = off; SET max_parallel_workers_per_gather = 0; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2r	{ UPDATE pn_v SET value = 11 FROM pn_i4 a JOIN pn_i4 b ON (a.f1 = (SELECT f1 FROM pn_i4 c WHERE c.f1 = b.f1)) WHERE ordnum = a.f1; }
step s2u	{ UPDATE pn_u SET n = pn_u.n + 1 FROM pn_i4 a, pn_i4 b WHERE pn_u.ordnum BETWEEN a.f1 AND a.f1 AND pn_u.k2 = b.f1 RETURNING pn_u.ordnum, pn_u.k2, pn_u.n, b.f1; }
step s2d	{ DELETE FROM pn_u USING pn_i4 a, pn_i4 b WHERE pn_u.ordnum BETWEEN a.f1 AND a.f1 AND pn_u.k2 = b.f1 RETURNING pn_u.ordnum, pn_u.k2, b.f1; }
step s2c	{ COMMIT; }

session s3
setup		{ SET enable_mergejoin = off; SET max_parallel_workers_per_gather = 0; }
step s3t	{ SELECT ttype, ordnum, partnum, value FROM pn_t ORDER BY ordnum; }
step s3u	{ SELECT ordnum, k2, n FROM pn_u ORDER BY ordnum; }
step s3pr	{ EXPLAIN (COSTS OFF) UPDATE pn_v SET value = 11 FROM pn_i4 a JOIN pn_i4 b ON (a.f1 = (SELECT f1 FROM pn_i4 c WHERE c.f1 = b.f1)) WHERE ordnum = a.f1; }
step s3pu	{ EXPLAIN (COSTS OFF) UPDATE pn_u SET n = pn_u.n + 1 FROM pn_i4 a, pn_i4 b WHERE pn_u.ordnum BETWEEN a.f1 AND a.f1 AND pn_u.k2 = b.f1; }
step s3pd	{ EXPLAIN (COSTS OFF) DELETE FROM pn_u USING pn_i4 a, pn_i4 b WHERE pn_u.ordnum BETWEEN a.f1 AND a.f1 AND pn_u.k2 = b.f1; }

permutation s3pr s3pu s3pd s1c s2c
permutation s1p s1k s2r s1c s2c s3t
permutation s1t s1b s2r s1c s2c s3t
permutation s2r s1p s2c s1c s3t
permutation s1n s1m s1j s1d s2u s1c s2c s3u
permutation s1n s1j s1d s1b s2d s1c s2c s3u

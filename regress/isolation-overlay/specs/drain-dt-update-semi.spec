# drain (gen mt-0076 group): an UPDATE or DELETE whose rows are a Hash Right Semi Join's first matches (`WHERE id IN
# (SELECT id FROM (a window subquery) x WHERE rn <= 1)`: the target hashed, the subquery's rows probing, x carried whole by
# a COPY mark) under a concurrent change of a matched target row. The victim (s2) blocks on the row s1 changed; after s1
# commits, PostgreSQL's EvalPlanQual reruns the join over the newest version and x's stored row (the window never runs
# again):
#  (a) s1 updated the row's value: s2 acts on the newest version
#  (b) s1 changed the row's ranking column: s2 still acts (x's stored row still names the row's unchanged id)
#  (c) s1 changed the row's id: x's stored row no longer matches the newest version: s2 skips it
#  (d) s1 deleted the row: s2 skips it
# Expected output is PostgreSQL 18.6's (the C oracle).

setup
{
 CREATE TABLE dds_t (id int PRIMARY KEY, n int NOT NULL, grp int, upd int DEFAULT 0);
 INSERT INTO dds_t SELECT g, g, g % 13, 0 FROM generate_series(1, 3000) g;
 ANALYZE dds_t;
}

teardown
{
 DROP TABLE dds_t;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE dds_t SET upd = upd + 100 WHERE id = 3000; }
step s1n	{ UPDATE dds_t SET n = 0 WHERE id = 3000; }
step s1id	{ UPDATE dds_t SET id = 9000 WHERE id = 3000; }
step s1del	{ DELETE FROM dds_t WHERE id = 3000; }
step s1c	{ COMMIT; }

session s2
setup
{
 SET enable_mergejoin = off;
 SET enable_nestloop = off;
}
step s2plan	{ EXPLAIN (costs off) UPDATE dds_t SET upd = -1 WHERE id IN (SELECT id FROM (SELECT id, row_number() OVER (PARTITION BY grp ORDER BY n DESC) rn FROM dds_t) x WHERE rn <= 1); }
step s2upd	{ UPDATE dds_t SET upd = -1 WHERE id IN (SELECT id FROM (SELECT id, row_number() OVER (PARTITION BY grp ORDER BY n DESC) rn FROM dds_t) x WHERE rn <= 1); }
step s2del	{ DELETE FROM dds_t WHERE id IN (SELECT id FROM (SELECT id, row_number() OVER (PARTITION BY grp ORDER BY n DESC) rn FROM dds_t) x WHERE rn <= 1); }
step s2chk	{ SELECT count(*), sum(upd), count(*) FILTER (WHERE upd = -1) FROM dds_t; }
step s2row	{ SELECT * FROM dds_t WHERE id IN (3000, 9000); }

permutation s2plan
permutation s1v s2upd s1c s2chk s2row
permutation s1n s2upd s1c s2chk s2row
permutation s1id s2upd s1c s2chk s2row
permutation s1del s2upd s1c s2chk s2row
permutation s1v s2del s1c s2chk s2row

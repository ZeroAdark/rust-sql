# M8-WRITE W5-J (D652-J): EvalPlanQual under a write whose rows come through a producer DAG: an Agg / Unique over a
# scan under a COPY-marked subquery (IN (SELECT DISTINCT ..), a GROUP BY subquery, a CTE's max()). The victim (s2)
# blocks on a target row s1 changed; after s1 commits, the recheck reruns every rerun producer and the write's pipeline
# over the target's newest version, the subquery's row as stored (ROW_MARK_COPY). Arms:
#  (a) the target row's v changed: the newest version is written
#  (b) the target row's join key changed: the recheck no longer joins, the row is skipped
#  (c) the subquery's source rows changed too: the recheck joins the stored subquery row, not a re-evaluated one
#  (d) a GROUP BY subquery joined by the target, under (a)
#  (e) a CTE's max() over the target's own table, the max row changed by s1
# Expected output is PostgreSQL 18.6's (the C oracle).

setup
{
 CREATE TABLE dt (id int, a int, v int);
 CREATE TABLE ds (a int, b int);
 INSERT INTO dt VALUES (1, 1, 10), (2, 2, 20), (3, 3, 30), (4, 4, 40), (5, 5, 50);
 INSERT INTO ds VALUES (1, 1), (1, 1), (2, 2), (4, 1), (5, 9);
 ANALYZE dt, ds;
}

teardown
{
 DROP TABLE dt, ds;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE dt SET v = v + 100 WHERE id = 1; }
step s1key	{ UPDATE dt SET a = 99 WHERE id = 1; }
step s1src	{ UPDATE dt SET v = v + 100 WHERE id = 1; UPDATE ds SET b = 7 WHERE a = 1; }
step s1max	{ UPDATE dt SET v = v + 100 WHERE id = 5; }
step s1c	{ COMMIT; }

session s2
setup
{
 SET enable_mergejoin = off;
 SET enable_nestloop = off;
}
step s2cnt	{ SELECT count(*) FROM dt; SELECT count(*) FROM ds; }
step s2plan	{ EXPLAIN (costs off) UPDATE dt SET v = v * 10 WHERE a IN (SELECT DISTINCT a FROM ds WHERE b = 1); }
step s2in	{ UPDATE dt SET v = v * 10 WHERE a IN (SELECT DISTINCT a FROM ds WHERE b = 1); }
step s2grp	{ UPDATE dt SET v = dt.v + g.n FROM (SELECT a, count(*) AS n FROM ds GROUP BY a) g WHERE dt.a = g.a; }
step s2cte	{ WITH m AS (SELECT max(id) AS maxid FROM dt) UPDATE dt SET v = v * 10 FROM m WHERE dt.id = m.maxid; }
step s2chk	{ SELECT * FROM dt ORDER BY id; }

permutation s2cnt s2plan s1c
permutation s1v s2in s1c s2chk
permutation s1key s2in s1c s2chk
permutation s1src s2in s1c s2chk
permutation s1v s2grp s1c s2chk
permutation s1max s2cte s1c s2chk

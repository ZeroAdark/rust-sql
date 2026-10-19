# EvalPlanQualStart (execMain.c:3119-3128) initializes fresh subplan
# planstates for the recheck: a volatile uncorrelated SubPlan runs again
# under EPQ instead of replaying its Material from the first pass. The
# sequence witnesses the second run (last_value 2). Expected output is the
# C oracle's (PostgreSQL 18.6).

setup
{
 CREATE TABLE epqsp (id int PRIMARY KEY, v int);
 INSERT INTO epqsp VALUES (1, 0);
 CREATE SEQUENCE epqsp_seq;
}

teardown
{
 DROP TABLE epqsp;
 DROP SEQUENCE epqsp_seq;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1u	{ UPDATE epqsp SET v = 1 WHERE id = 1; }
step s1c	{ COMMIT; }

session s2
step s2u	{ UPDATE epqsp SET v = 100 WHERE id <= ALL (SELECT nextval('epqsp_seq')); }
step s2s	{ SELECT last_value FROM epqsp_seq; }
step s2v	{ SELECT id, v FROM epqsp; }

permutation s1u s2u s1c s2s s2v

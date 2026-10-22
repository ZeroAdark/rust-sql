# semantics D2028 (recount 2026-10-04 witness d2028, from the filing's scratch e2e zz_d652li_epq forms w1 / w2): a write's
# EvalPlanQual beside an InitPlan whose producer (an Agg over a scan) calls a VOLATILE function. PostgreSQL 18.6's
# EvalPlanQualStart copies an evaluated InitPlan's param and never runs it again (w1: nextval 3), and forces a pending one
# the subplan's extParam holds (w2: the InitPlan sits in an OR arm the first pass skips; the recheck forces it: nextval 3).
# w3: an InitPlan only RETURNING reads is no subplan param, so the recheck never forces it, and the rechecked row fails
# the WHERE: no row returns and the InitPlan never runs (nextval not called). Before the fix the runner's recheck reran the
# Init's producer (w1: nextval 6).
setup
{
 CREATE TABLE w6q (id int PRIMARY KEY, c int);
 INSERT INTO w6q SELECT i, i FROM generate_series(1, 8) i;
 CREATE TABLE w6k (k int);
 INSERT INTO w6k SELECT i FROM generate_series(1, 3) i;
 CREATE SEQUENCE w6_seq;
 CREATE FUNCTION w6_bump(int) RETURNS boolean LANGUAGE plpgsql VOLATILE AS $$ BEGIN PERFORM nextval('w6_seq'); RETURN $1 > 0; END $$;
 ANALYZE w6q, w6k;
}

teardown
{
 DROP TABLE w6q, w6k;
 DROP FUNCTION w6_bump(int);
 DROP SEQUENCE w6_seq;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step b3		{ UPDATE w6q SET c = c + 10 WHERE id = 3; }
step b5		{ UPDATE w6q SET c = c + 10 WHERE id = 5; }
step bc		{ COMMIT; }

session s2
step w1		{ UPDATE w6q SET c = c + 1 WHERE id <= 3 AND c < (SELECT max(k) + 100 FROM w6k WHERE w6_bump(k)); }
step w2		{ UPDATE w6q SET c = c + 1 WHERE id = 5 AND (id > 4 OR c < (SELECT max(k) + 100 FROM w6k WHERE w6_bump(k))); }
step w3		{ UPDATE w6q SET c = c + 1 WHERE id = 5 AND c < 10 RETURNING id, (SELECT max(k) FROM w6k WHERE w6_bump(k)) AS m; }
step chk	{ SELECT id, c FROM w6q ORDER BY id; }
step seq	{ SELECT last_value, is_called FROM w6_seq; }

permutation b3 w1 bc chk seq
permutation b5 w2 bc chk seq
permutation b5 w3 bc chk seq

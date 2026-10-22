# D3498: an UPDATE / DELETE whose qual reads a per-row SubPlan over a plan Material of a Result that calls nextval.
# PostgreSQL 18.6's Material makes the row at the SubPlan's first evaluation and keeps it for the statement's later rows;
# EvalPlanQualStart initializes the SubPlan afresh in the recheck's EState without EXEC_FLAG_REWIND, so each recheck runs
# the Result again (one nextval per recheck), while the rows after it read the statement's own store; a recheck whose
# new version fails a qual before the SubPlan runs none. The sequence and the rows written witness both. Expected output
# is the C oracle's (PostgreSQL 18.6).

setup
{
 CREATE TABLE sr_t (id int PRIMARY KEY, v int);
 INSERT INTO sr_t SELECT g, 0 FROM generate_series(1, 4) g;
 CREATE SEQUENCE sr_q;
}

teardown
{
 DROP TABLE sr_t;
 DROP SEQUENCE sr_q;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE sr_t SET v = 1 WHERE id = 1; }
step s1key	{ UPDATE sr_t SET id = 7 WHERE id = 1; }
step s1two	{ UPDATE sr_t SET v = v + 1 WHERE id IN (1, 2); }
step s1c	{ COMMIT; }

session s2
step s2u	{ UPDATE sr_t SET v = 100 WHERE id <= ALL (SELECT nextval('sr_q')); }
step s2u1	{ UPDATE sr_t SET v = 100 WHERE id <= ALL (SELECT nextval('sr_q') + 1); }
step s2d	{ DELETE FROM sr_t WHERE id <= ALL (SELECT nextval('sr_q')) RETURNING id, v; }
step s2f	{ UPDATE sr_t SET v = 100 WHERE v = 0 AND id <= ALL (SELECT nextval('sr_q')); }
step s2s	{ SELECT last_value FROM sr_q; }
step s2v	{ SELECT id, v FROM sr_t ORDER BY id; }

permutation s1v s2u s1c s2s s2v
permutation s1key s2u s1c s2s s2v
permutation s1v s2d s1c s2s s2v
permutation s1two s2u1 s1c s2s s2v
permutation s1v s2f s1c s2s s2v
permutation s1key s2f s1c s2s s2v

# D3503: an UPDATE / DELETE whose ANY sublink over a FROM-less subquery (nextval in its target list) is a Nested Loop
# Semi Join over a Materialize of the Subquery Scan of a Result, the subquery COPY-marked, while another session changes
# a matched row (READ COMMITTED). EvalPlanQual rechecks the newest version against the subquery's stored row, never
# running nextval again: the sequence reads one call. Forms: an in-place update (the write runs over the newest
# version), a re-key that no longer passes the ANY comparison (no write), a delete (no write), the DELETE form.
# Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE sr_t (id int PRIMARY KEY, v int);
 INSERT INTO sr_t SELECT g, 0 FROM generate_series(1, 6) g;
 CREATE SEQUENCE sr_q;
}

teardown
{
 DROP TABLE sr_t;
 DROP SEQUENCE sr_q;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE sr_t SET v = v + 100 WHERE id = 5; }
step s1key	{ UPDATE sr_t SET id = -5 WHERE id = 5; }
step s1del	{ DELETE FROM sr_t WHERE id = 5; }
step s1c	{ COMMIT; }

session s2
step s2exp	{ EXPLAIN (COSTS OFF) UPDATE sr_t SET v = v + 1 WHERE id > ANY (SELECT nextval('sr_q') + 2) RETURNING id, v; }
step s2u	{ UPDATE sr_t SET v = v + 1 WHERE id > ANY (SELECT nextval('sr_q') + 2) RETURNING id, v; }
step s2d	{ DELETE FROM sr_t WHERE id > ANY (SELECT nextval('sr_q') + 2) RETURNING id, v; }
step s2sel	{ SELECT id, v FROM sr_t ORDER BY id; }
step s2seq	{ SELECT last_value FROM sr_q; }

permutation s2exp s1v s2u s1c s2sel s2seq
permutation s1key s2u s1c s2sel s2seq
permutation s1del s2u s1c s2sel s2seq
permutation s1v s2d s1c s2sel s2seq

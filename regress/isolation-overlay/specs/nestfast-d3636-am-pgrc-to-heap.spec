# D3636 (nestfast; D3622's re-land): a routed INSERT admitted while its leaf rq1 is a pgrcolumnar table (the
# concurrent ALTER .. SET ACCESS METHOD heap not yet committed), whose first row reaches rq1 after that ALTER commits.
# The leaf is a heap table under its lock at that row, so it is written as PostgreSQL writes it. PGRCSERVE M1: a leaf
# still pgrc at its first row is written through its table AM (the last permutation). In PostgreSQL the AM is heap's
# handler under another name and the rows stand. isowrite-am-routed-leaf is the other direction (heap at admission).
setup
{
 DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_am WHERE amname = 'pgrcolumnar') THEN CREATE ACCESS METHOD pgrcolumnar TYPE TABLE HANDLER heap_tableam_handler; END IF; END $$;
 CREATE TABLE rq (a int, b text) PARTITION BY LIST (a);
 CREATE TABLE rq1 PARTITION OF rq FOR VALUES IN (1) USING pgrcolumnar;
 CREATE TABLE rq2 PARTITION OF rq FOR VALUES IN (2);
}
teardown
{
 DROP TABLE rq;
 DROP ACCESS METHOD IF EXISTS pgrcolumnar;
}
session s1
step s1b	{ BEGIN; }
step s1am	{ ALTER TABLE rq1 SET ACCESS METHOD heap; }
step s1c	{ COMMIT; }
session s2
step s2i	{ INSERT INTO rq VALUES (1, 'x'); }
step s2j	{ INSERT INTO rq SELECT g % 2 + 1, 'y' || g FROM generate_series(1, 4) g; }
session s3
step s3s	{ SELECT tableoid::regclass, * FROM rq ORDER BY a, b; }
permutation s1b s1am s2i s1c s3s
permutation s1b s1am s2j s1c s3s
permutation s2j s3s

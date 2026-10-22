# producerchain E: a row lock over a partitioned table whose rows feed another node (an aggregate), not the client, meets
# a row s1 changed. The temporal foreign key check over a partitioned referenced table is that form: `SELECT 1 FROM
# (SELECT valid_at AS r FROM parent x WHERE id = $1 AND valid_at && $2 FOR KEY SHARE OF x) x1 HAVING $2 <@
# range_agg(x1.r)`, its plan pruned to the one partition the key names. LockRows locks each row in its own partition;
# when s1 changed the row's key, EvalPlanQual reruns that partition's scan over the newest version alone and hands the
# version that still passes to the aggregate (a deleted row, or one that moved off the quals, is skipped). So the child
# row's insert sees s1's shrunk or deleted range and fails, or its widened range and succeeds. The same holds for a count
# and sum over `FOR UPDATE` of one partition and over the whole table (each partition an Append arm). A row s1 moved to
# another partition raises the moved-partition error.

setup
{
 CREATE TABLE fp (id int4range, valid_at daterange, name text, CONSTRAINT fp_pk PRIMARY KEY (id, valid_at WITHOUT OVERLAPS)) PARTITION BY LIST (id);
 CREATE TABLE fp1 PARTITION OF fp FOR VALUES IN ('[1,2)', '[3,4)');
 CREATE TABLE fp2 PARTITION OF fp FOR VALUES IN ('[2,3)', '[4,5)');
 INSERT INTO fp VALUES ('[1,2)', daterange('2000-01-01', '2000-02-01'), 'a'), ('[1,2)', daterange('2000-02-01', '2000-03-01'), 'b'), ('[2,3)', daterange('2000-01-01', '2010-01-01'), 'c');
 CREATE TABLE fc (id int4range, valid_at daterange, parent_id int4range, CONSTRAINT fc_pk PRIMARY KEY (id, valid_at WITHOUT OVERLAPS), CONSTRAINT fc_fk FOREIGN KEY (parent_id, PERIOD valid_at) REFERENCES fp (id, PERIOD valid_at));
 CREATE TABLE lp (id int, v int) PARTITION BY LIST (id);
 CREATE TABLE lp1 PARTITION OF lp FOR VALUES IN (1, 3);
 CREATE TABLE lp2 PARTITION OF lp FOR VALUES IN (2, 4);
 INSERT INTO lp VALUES (1, 1), (1, 2), (2, 3), (3, 4), (4, 5);
}

teardown
{
 DROP TABLE fc;
 DROP TABLE fp;
 DROP TABLE lp;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1k	{ UPDATE fp SET valid_at = daterange('2000-01-01', '2000-01-20') WHERE name = 'a'; }
step s1w	{ UPDATE fp SET valid_at = daterange('1999-12-01', '2000-02-01') WHERE name = 'a'; }
step s1d	{ DELETE FROM fp WHERE name = 'b'; }
step s1v	{ UPDATE lp SET v = v + 10 WHERE id = 1 AND v = 1; }
step s1q	{ UPDATE lp SET v = 100 WHERE id = 2; }
step s1m	{ UPDATE lp SET id = 2 WHERE id = 1 AND v = 2; }
step s1c	{ COMMIT; }

session s2
setup		{ SET max_parallel_workers_per_gather = 0; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2i	{ INSERT INTO fc VALUES ('[1,2)', daterange('2000-01-10', '2000-02-15'), '[1,2)'); }
step s2a	{ SELECT count(*), sum(v) FROM (SELECT v FROM lp WHERE id = 1 AND v < 50 FOR UPDATE) s; }
step s2b	{ SELECT count(*), sum(v) FROM (SELECT v FROM lp WHERE v < 50 FOR UPDATE) s; }
step s2s	{ SELECT * FROM fc ORDER BY id, valid_at; }
step s2c	{ COMMIT; }

permutation s1k s2i s1c s2s s2c
permutation s1w s2i s1c s2s s2c
permutation s1d s2i s1c s2s s2c
permutation s1v s2a s1c s2c
permutation s1q s2b s1c s2c
permutation s1v s2b s1c s2c
permutation s1m s2a s1c s2c

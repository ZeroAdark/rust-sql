# D699: a served statement that returns rows and meets a concurrent update mid-scan (EvalPlanQual, READ COMMITTED) sends
# the rows before the rechecked one first, then the rechecked row, then the rest: PostgreSQL's scan order. lockix: an id
# range with seq scans off (a Bitmap Heap Scan without statistics) through the runner, one statement (D696's bitmap walk)
setup
{
 CREATE TABLE d699 (id int PRIMARY KEY, v int NOT NULL);
 INSERT INTO d699 SELECT g, g * 10 FROM generate_series(1, 10) g;
}
teardown { DROP TABLE d699; }
session s1
setup { BEGIN ISOLATION LEVEL READ COMMITTED; }
step up5 { UPDATE d699 SET v = v + 1000 WHERE id = 5; }
step out5 { UPDATE d699 SET v = -1 WHERE id = 5; }
step c1 { COMMIT; }
session s2
setup { BEGIN ISOLATION LEVEL READ COMMITTED; }
step lock { SELECT id, v FROM d699 WHERE v > 0 FOR UPDATE; }
step share { SELECT id, v FROM d699 WHERE v > 0 FOR SHARE; }
step updret { UPDATE d699 SET v = v + 1 WHERE v > 0 RETURNING id, v; }
step delret { DELETE FROM d699 WHERE v > 0 RETURNING id, v; }
step c2 { COMMIT; }
session s4
setup { BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL enable_seqscan = off; }
step lockix { SELECT id, v FROM d699 WHERE id BETWEEN 2 AND 8 FOR UPDATE; }
step plan4 { EXPLAIN (COSTS OFF) SELECT id, v FROM d699 WHERE id BETWEEN 2 AND 8 FOR UPDATE; }
step c4 { COMMIT; }
session s3
step read { SELECT * FROM d699 ORDER BY id; }
permutation up5 lock c1 c2 read c4
permutation out5 lock c1 c2 read c4
permutation plan4 c4 c2 c1
permutation up5 lockix c1 c4 read c2
permutation up5 share c1 c2 read c4
permutation up5 updret c1 c2 read c4
permutation up5 delret c1 c2 read c4

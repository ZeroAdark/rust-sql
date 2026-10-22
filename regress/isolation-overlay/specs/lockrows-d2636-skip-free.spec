# lockrows D2636: an UPDATE / DELETE ... WHERE id IN (SELECT id FROM (SELECT id FROM (window subquery) x WHERE rn <= 3
# OFFSET 0) s) planned over the unique-ified subquery: a HashAggregate grouped by s.id de-duplicates the subquery's rows and
# hands on s.*, the subquery row the write's ROW_MARK_COPY stores for EvalPlanQual. The recheck never re-evaluates the
# subquery: its producers (the window's Sort, the window's Materialize, the HashAggregate) are skipped and their slots open
# empty, and a skipped producer ends the lives of the slots it was the last reader of as a run does, so the HashAggregate's
# group table opens on the slot number the Sort used (the hash-join and nested-loop forms raised "slot 0 produced again
# ... (L3)" before). Every id appears twice among the subquery's rows, with different row numbers, so the stored row is one
# member of its group. The forms: a merge join over the sorted unique-ified rows (s2), a hash join probing them (s4), a
# nested loop over them (s5). s1 changes target rows the subquery picked while the write waits: PG writes the newest
# version of a row whose key still matches, skips a moved or deleted one, and does not see s1's change to the subquery's
# source (the stored row is handed to the recheck, the subquery is not evaluated again).
setup
{
 CREATE TABLE cik (id int PRIMARY KEY, g int NOT NULL, n int NOT NULL, val int);
 INSERT INTO cik SELECT i, i % 7, i, i FROM generate_series(1, 5000) i;
 CREATE TABLE cis (id int NOT NULL, g int NOT NULL, n int NOT NULL);
 INSERT INTO cis SELECT i % 20 + 1, i % 13, i FROM generate_series(1, 600) i;
 ANALYZE cik, cis;
}

teardown
{
 DROP TABLE cik;
 DROP TABLE cis;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE cik SET val = val + 5 WHERE id = 7; }
step s1k	{ UPDATE cik SET id = 7008 WHERE id = 8; }
step s1d	{ DELETE FROM cik WHERE id = 9; }
step s1s	{ UPDATE cis SET g = 99 WHERE n IN (3, 16, 29); }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2p	{ EXPLAIN (COSTS OFF) UPDATE cik SET val = val + 1000 WHERE id IN (SELECT id FROM (SELECT id FROM (SELECT id, row_number() OVER (PARTITION BY g ORDER BY n) rn FROM cis) x WHERE rn <= 3 OFFSET 0) s); }
step s2u	{ UPDATE cik SET val = val + 1000 WHERE id IN (SELECT id FROM (SELECT id FROM (SELECT id, row_number() OVER (PARTITION BY g ORDER BY n) rn FROM cis) x WHERE rn <= 3 OFFSET 0) s); }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT id, val FROM cik WHERE id IN (6, 7, 8, 9, 10, 20, 21, 7008) ORDER BY id; }

session s4
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s4p	{ EXPLAIN (COSTS OFF) DELETE FROM cik WHERE id IN (SELECT id FROM (SELECT id FROM (SELECT id, row_number() OVER (PARTITION BY g ORDER BY n) rn FROM cis) x WHERE rn <= 3 OFFSET 0) s); }
step s4d	{ DELETE FROM cik WHERE id IN (SELECT id FROM (SELECT id FROM (SELECT id, row_number() OVER (PARTITION BY g ORDER BY n) rn FROM cis) x WHERE rn <= 3 OFFSET 0) s); }
step s4c	{ COMMIT; }

session s5
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s5p	{ EXPLAIN (COSTS OFF) UPDATE cik SET n = -n WHERE id IN (SELECT id FROM (SELECT id FROM (SELECT id, row_number() OVER (PARTITION BY g ORDER BY n) rn FROM cis) x WHERE rn <= 3 OFFSET 0) s); }
step s5u	{ UPDATE cik SET val = -val WHERE id IN (SELECT id FROM (SELECT id FROM (SELECT id, row_number() OVER (PARTITION BY g ORDER BY n) rn FROM cis) x WHERE rn <= 3 OFFSET 0) s); }
step s5c	{ COMMIT; }

session s6
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s6p	{ EXPLAIN (COSTS OFF) UPDATE cik SET val = val + 100 WHERE id IN (SELECT id FROM (SELECT id FROM (SELECT id, row_number() OVER (PARTITION BY g ORDER BY n) rn FROM cis) x WHERE rn <= 3 OFFSET 0) s); }
step s6u	{ UPDATE cik SET val = val + 100 WHERE id IN (SELECT id FROM (SELECT id FROM (SELECT id, row_number() OVER (PARTITION BY g ORDER BY n) rn FROM cis) x WHERE rn <= 3 OFFSET 0) s); }
step s6c	{ COMMIT; }

permutation s2p s4p s5p s6p s1c s2c s4c s5c s6c
permutation s1v s1k s1d s1s s2u s1c s2c s4c s5c s6c s3sel
permutation s1v s1k s1d s1s s4d s1c s4c s2c s5c s6c s3sel
permutation s1v s1k s1d s1s s5u s1c s5c s2c s4c s6c s3sel
permutation s1v s1k s1d s1s s6u s1c s6c s2c s4c s5c s3sel

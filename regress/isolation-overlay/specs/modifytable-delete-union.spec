# DELETE-UNION (modifytable): a DELETE of several result relations (a list-partitioned table whose leaf keeps its columns
# in another order, an inheritance tree) whose rows come out of one settled union of the targets' scans, hashed on the
# join's build side while the USING relation's rows probe it, meets a target row s1
# changed. PostgreSQL takes each row's result relation from its junk tableoid and its position from its junk ctid;
# EvalPlanQual substitutes that relation's scan with the newest version, blocks every other result relation's scan and
# refetches the USING relation's row at its junk ctid, so only that version is joined again: one that still joins is
# deleted, one whose join key s1 moved or that s1 deleted is skipped, the other relations' rows are not deleted twice,
# and a row s1 moved to another partition raises 40001.

setup
{
 CREATE TABLE dun_p (region text NOT NULL, id int NOT NULL, v int) PARTITION BY LIST (region);
 CREATE TABLE dun_p_eu PARTITION OF dun_p FOR VALUES IN ('eu');
 CREATE TABLE dun_p_us (v int, id int NOT NULL, region text NOT NULL);
 ALTER TABLE dun_p ATTACH PARTITION dun_p_us FOR VALUES IN ('us');
 CREATE TABLE dun_p_jp PARTITION OF dun_p FOR VALUES IN ('jp');
 INSERT INTO dun_p SELECT (ARRAY['eu', 'us', 'jp'])[1 + i % 3], i, i * 10 FROM generate_series(1, 30) i;
 CREATE TABLE dun_s (id int, tag text);
 INSERT INTO dun_s SELECT i, CASE WHEN i % 2 = 0 THEN 'go' ELSE 'stay' END FROM generate_series(1, 400) i;
 CREATE TABLE dun_par (f1 int, f2 text);
 CREATE TABLE dun_cld (z int, f2 text, f1 int);
 ALTER TABLE dun_cld INHERIT dun_par;
 INSERT INTO dun_par SELECT i, 'p' || i FROM generate_series(1, 10) i;
 INSERT INTO dun_cld SELECT 100 + i, 'c' || i, 10 + i FROM generate_series(1, 10) i;
 ANALYZE dun_p, dun_s, dun_par, dun_cld;
}

teardown
{
 DROP TABLE dun_cld;
 DROP TABLE dun_par;
 DROP TABLE dun_s;
 DROP TABLE dun_p;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE dun_p SET v = v + 1 WHERE id IN (2, 4, 6); }
step s1k	{ UPDATE dun_p SET id = id + 1 WHERE id = 8; }
step s1d	{ DELETE FROM dun_p WHERE id = 10; }
step s1m	{ UPDATE dun_p SET region = 'jp' WHERE id = 12; }
step s1i	{ UPDATE dun_cld SET f2 = f2 || '*' WHERE f1 IN (12, 14); }
step s1j	{ UPDATE dun_cld SET f1 = 13 WHERE f1 = 16; }
step s1c	{ COMMIT; }

session s2
setup		{ SET max_parallel_workers_per_gather = 0; SET enable_nestloop = off; SET enable_mergejoin = off; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2h	{ DELETE FROM dun_p USING dun_s s WHERE dun_p.id = s.id AND s.tag = 'go'; }
step s2i	{ DELETE FROM dun_par USING dun_s s WHERE dun_par.f1 = s.id AND s.tag = 'go'; }
step s2c	{ COMMIT; }

session s3
setup		{ SET max_parallel_workers_per_gather = 0; SET enable_nestloop = off; SET enable_mergejoin = off; }
step s3p	{ SELECT tableoid::regclass, id, v FROM dun_p ORDER BY id, v; }
step s3i	{ SELECT tableoid::regclass, f1, f2 FROM dun_par ORDER BY f1; }
step s3e	{ EXPLAIN (COSTS OFF) DELETE FROM dun_p USING dun_s s WHERE dun_p.id = s.id AND s.tag = 'go'; }

permutation s3e s1v s1k s1d s2h s1c s2c s3p
permutation s1m s2h s1c s2c s3p
permutation s1i s1j s2i s1c s2c s3i

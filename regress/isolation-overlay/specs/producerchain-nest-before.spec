# D3486: producerchain C's nest write (lowerv4 nest_outer.rs: the target's own scan the outer of nested loops over a
# Material, INNER or LEFT) into a target with BEFORE ROW triggers on the command. PostgreSQL's EvalPlanQual then runs in
# GetTupleForTrigger: s2's UPDATE / DELETE waits on the rows s1 changed; after s1 commits, the newest version is locked
# and rechecked (the joins again over it, every other relation's row refetched at its junk ctid) before any trigger
# runs, so a row whose key s1 moved off the joined row and a row s1 deleted fire no trigger and are skipped, and a row
# s1 changed in another column fires each trigger once, OLD the newest version (the WHEN clause and the trigger's NEW
# edits read it). A trigger returning NULL skips its row; RETURNING lists the rows as written. With s1 rolled back, s2
# writes the old versions.

setup
{
 CREATE TABLE bp (pid int, txt text);
 INSERT INTO bp VALUES (1, 'p1'), (2, 'p2'), (5, 'p5'), (6, 'p6'), (8, 'p8');
 CREATE TABLE bc (pid int, descrip text, n int NOT NULL DEFAULT 0, ver int NOT NULL DEFAULT 0);
 INSERT INTO bc (pid, descrip) VALUES (1, 'c1'), (2, 'c2'), (5, 'c5'), (5, 'c5b'), (6, 'c6'), (8, 'c8');
 CREATE TABLE bq (pid int, tag text);
 INSERT INTO bq VALUES (1, 'q1'), (5, 'q5'), (6, 'q6');
 CREATE FUNCTION bc_note() RETURNS trigger LANGUAGE plpgsql AS $$
 BEGIN
   IF TG_OP = 'UPDATE' THEN
     RAISE NOTICE 'before % % old pid=% n=% ver=% new n=%', TG_OP, OLD.descrip, OLD.pid, OLD.n, OLD.ver, NEW.n;
   ELSE
     RAISE NOTICE 'before % % old pid=% n=% ver=%', TG_OP, OLD.descrip, OLD.pid, OLD.n, OLD.ver;
   END IF;
   IF OLD.descrip = 'c8' THEN
     RETURN NULL;
   END IF;
   IF TG_OP = 'DELETE' THEN
     RETURN OLD;
   END IF;
   RETURN NEW;
 END $$;
 CREATE FUNCTION bc_touch() RETURNS trigger LANGUAGE plpgsql AS $$
 BEGIN
   NEW.ver := OLD.ver + 1;
   RETURN NEW;
 END $$;
 CREATE TRIGGER bc_a_note BEFORE UPDATE OR DELETE ON bc FOR EACH ROW EXECUTE FUNCTION bc_note();
 CREATE TRIGGER bc_b_touch BEFORE UPDATE ON bc FOR EACH ROW WHEN (OLD.n IS DISTINCT FROM NEW.n) EXECUTE FUNCTION bc_touch();
}

teardown
{
 DROP TABLE bp;
 DROP TABLE bc;
 DROP TABLE bq;
 DROP FUNCTION bc_note();
 DROP FUNCTION bc_touch();
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1n	{ UPDATE bc SET n = n + 100 WHERE descrip = 'c6'; }
step s1k	{ UPDATE bc SET pid = 7 WHERE descrip = 'c5'; }
step s1d	{ DELETE FROM bc WHERE descrip = 'c2'; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET max_parallel_workers_per_gather = 0; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2u	{ UPDATE bc SET n = bc.n + 1 FROM bp LEFT JOIN bq USING (pid) WHERE bc.pid = bp.pid AND bp.txt IN ('p2', 'p5', 'p6', 'p8') RETURNING bc.pid, bc.descrip, bc.n, bc.ver, bq.tag; }
step s2d	{ DELETE FROM bc USING bp LEFT JOIN bq USING (pid) WHERE bc.pid = bp.pid AND bp.txt IN ('p2', 'p5', 'p6', 'p8') RETURNING bc.pid, bc.descrip, bq.tag; }
step s2c	{ COMMIT; }

session s3
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET max_parallel_workers_per_gather = 0; }
step s3sel	{ SELECT pid, descrip, n, ver FROM bc ORDER BY pid, descrip; }
step s3pu	{ EXPLAIN (COSTS OFF) UPDATE bc SET n = bc.n + 1 FROM bp LEFT JOIN bq USING (pid) WHERE bc.pid = bp.pid AND bp.txt IN ('p2', 'p5', 'p6', 'p8'); }
step s3pd	{ EXPLAIN (COSTS OFF) DELETE FROM bc USING bp LEFT JOIN bq USING (pid) WHERE bc.pid = bp.pid AND bp.txt IN ('p2', 'p5', 'p6', 'p8'); }

permutation s3pu s3pd s1c s2c
permutation s1n s1k s1d s2u s1c s2c s3sel
permutation s1n s1k s1d s2d s1c s2c s3sel
permutation s1n s1k s1d s2u s1a s2c s3sel
permutation s2u s1n s2c s1c s3sel

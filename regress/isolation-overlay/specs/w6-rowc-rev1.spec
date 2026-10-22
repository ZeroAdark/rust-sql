# W6-D655-ROWC (V4R-534) kit, from code review round 1 (correctness lens, 21:1xZ): EvalPlanQual probes beyond w6-rowc. A VOLATILE
# qual rechecked; a LIMIT 1 reader with a VOLATILE RETURNING (the recheck at the first row, and at the last during the drain);
# a SET and a qual that raise only over the newest version; a generic plan's $1 in the index recheck (fails / passes);
# AFTER ROW + transition-table triggers over a rechecked UPDATE and DELETE; RETURNING ctid after the retry; the outer
# query's own scan of the target (statement snapshot) joined with the rechecked DELETE's RETURNING.

setup
{
 CREATE TABLE rv (id int PRIMARY KEY, grp int, v int, t text);
 INSERT INTO rv SELECT i, i % 3, i, 'row' || i FROM generate_series(1, 12) i;
 CREATE INDEX rv_v ON rv (v);
 CREATE SEQUENCE rvsq;
 CREATE TABLE rvt (id int PRIMARY KEY, grp int, v int);
 INSERT INTO rvt SELECT i, i % 3, i FROM generate_series(1, 6) i;
 CREATE FUNCTION rv_ar() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE NOTICE 'ar % id % old % new %', TG_OP, COALESCE(NEW.id, OLD.id), OLD.v, NEW.v; RETURN NULL; END $$;
 CREATE FUNCTION rv_as() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE NOTICE 'as % old rows % sum %', TG_OP, (SELECT count(*) FROM old_rows), (SELECT sum(v) FROM old_rows); RETURN NULL; END $$;
 CREATE TRIGGER rvt_ar AFTER UPDATE OR DELETE ON rvt FOR EACH ROW EXECUTE FUNCTION rv_ar();
 CREATE TRIGGER rvt_as AFTER UPDATE ON rvt REFERENCING OLD TABLE AS old_rows FOR EACH STATEMENT EXECUTE FUNCTION rv_as();
 ANALYZE rv, rvt;
}

teardown
{
 DROP TABLE rvt;
 DROP FUNCTION rv_ar(), rv_as();
 DROP SEQUENCE rvsq;
 DROP TABLE rv;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE rv SET v = v + 100 WHERE id = 3; }
step s1v12	{ UPDATE rv SET v = v + 100 WHERE id = 12; }
step s1t	{ UPDATE rv SET t = 'new' || id WHERE id = 3; }
step s1z	{ UPDATE rv SET v = 103 WHERE id = 6; }
step s1tv	{ UPDATE rvt SET v = v + 100 WHERE id = 3; }
step s1c	{ COMMIT; }

session s2
step s2b	{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2vq	{ WITH c AS (UPDATE rv SET v = v + 1 WHERE grp = 0 AND nextval('rvsq') > 0 RETURNING id, v) SELECT * FROM c ORDER BY id; }
step s2l1	{ WITH c AS (UPDATE rv SET v = v + 1 WHERE grp = 0 RETURNING id, v, nextval('rvsq') AS n) SELECT * FROM c LIMIT 1; }
step s2rz	{ WITH c AS (UPDATE rv SET v = 10 / (v - 103) WHERE grp = 0 RETURNING id, v) SELECT * FROM c ORDER BY id; }
step s2qz	{ WITH c AS (UPDATE rv SET v = v + 1 WHERE grp = 0 AND 10 / (v - 103) <> 7 RETURNING id, v) SELECT * FROM c ORDER BY id; }
step s2g	{ SET LOCAL plan_cache_mode = force_generic_plan; SET LOCAL enable_seqscan = off; SET LOCAL enable_bitmapscan = off; DEALLOCATE ALL; PREPARE p(int) AS WITH c AS (UPDATE rv SET grp = grp + 10 WHERE v = $1 RETURNING id, grp, v, t) SELECT * FROM c; EXECUTE p(3); }
step s2tr	{ WITH c AS (UPDATE rvt SET v = v + 1 WHERE grp = 0 RETURNING id, v) SELECT * FROM c ORDER BY id; }
step s2trd	{ WITH c AS (DELETE FROM rvt WHERE grp = 0 RETURNING id, v) SELECT * FROM c ORDER BY id; }
step s2ct	{ WITH c AS (UPDATE rv SET v = v + 1 WHERE id = 3 RETURNING id, v, ctid) SELECT * FROM c; }
step s2dj	{ WITH d AS (DELETE FROM rv WHERE grp = 0 RETURNING id, v) SELECT d.id, d.v, r.v AS snap_v FROM d JOIN rv r USING (id) ORDER BY 1; }
step s2seq	{ SELECT last_value, is_called FROM rvsq; }
step s2c	{ COMMIT; }

session s4
step s4sel	{ SELECT id, grp, v, t FROM rv ORDER BY id; }
step s4t	{ SELECT id, grp, v FROM rvt ORDER BY id; }
step s4seq	{ SELECT last_value, is_called FROM rvsq; }

permutation s2b s1v s2vq s1c s2seq s2c s4sel
permutation s2b s1v s2l1 s1c s2c s4sel s4seq
permutation s2b s1v12 s2l1 s1c s2c s4sel s4seq
permutation s2b s1z s2rz s1c s2c s4sel
permutation s2b s1z s2qz s1c s2c s4sel
permutation s2b s1v s2g s1c s2c s4sel
permutation s2b s1t s2g s1c s2c s4sel
permutation s2b s1tv s2tr s1c s2c s4t
permutation s2b s1tv s2trd s1c s2c s4t
permutation s2b s1v s2ct s1c s2c s4sel
permutation s2b s1v s2dj s1c s2c s4sel

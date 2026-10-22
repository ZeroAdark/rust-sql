# D2812 BEFORE ROW (D655 Row C): a data-modifying CTE's UPDATE / DELETE of one target that fires BEFORE ROW triggers
# rechecks in the IR. PostgreSQL's EvalPlanQual runs in GetTupleForTrigger: s2's CTE waits on the rows s1 changed; after
# s1 commits, the newest version is locked and rechecked before any trigger runs, so a row s1 moved out of the qual and a
# row s1 deleted fire no trigger, and a row s1 changed in another column fires each trigger once, OLD the newest version
# (the WHEN clause and the trigger's NEW edits read it). A trigger returning NULL skips its row; the RETURNING list and
# the INSERT reading it see the rows as written. With s1 rolled back, s2 writes the old versions.

setup
{
 CREATE TABLE rb (k int PRIMARY KEY, v int NOT NULL, grp int NOT NULL, bal int NOT NULL, ver int NOT NULL DEFAULT 0);
 INSERT INTO rb SELECT g, g, g % 5, 100 + g, 0 FROM generate_series(1, 50) g;
 CREATE TABLE rlog (k int);
 CREATE FUNCTION rb_note() RETURNS trigger LANGUAGE plpgsql AS $$
 BEGIN
   IF OLD.k IN (11, 21, 26, 31, 41) THEN
     IF TG_OP = 'UPDATE' THEN
       RAISE NOTICE 'before % k=% old v=% grp=% bal=% ver=% new v=% bal=%', TG_OP, OLD.k, OLD.v, OLD.grp, OLD.bal, OLD.ver, NEW.v, NEW.bal;
     ELSE
       RAISE NOTICE 'before % k=% old v=% grp=% bal=% ver=%', TG_OP, OLD.k, OLD.v, OLD.grp, OLD.bal, OLD.ver;
     END IF;
   END IF;
   IF OLD.k = 41 THEN
     RETURN NULL;
   END IF;
   IF TG_OP = 'DELETE' THEN
     RETURN OLD;
   END IF;
   RETURN NEW;
 END $$;
 CREATE FUNCTION rb_touch() RETURNS trigger LANGUAGE plpgsql AS $$
 BEGIN
   NEW.ver := OLD.ver + 1;
   RETURN NEW;
 END $$;
 CREATE TRIGGER rb_a_note BEFORE UPDATE OR DELETE ON rb FOR EACH ROW EXECUTE FUNCTION rb_note();
 CREATE TRIGGER rb_b_touch BEFORE UPDATE ON rb FOR EACH ROW WHEN (OLD.bal IS DISTINCT FROM NEW.bal) EXECUTE FUNCTION rb_touch();
}

teardown
{
 DROP TABLE rb;
 DROP TABLE rlog;
 DROP FUNCTION rb_note();
 DROP FUNCTION rb_touch();
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1mv	{ UPDATE rb SET grp = 9 WHERE k = 11; }
step s1v	{ UPDATE rb SET v = 1000 WHERE k = 21; }
step s1b	{ UPDATE rb SET bal = bal + 5 WHERE k = 26; }
step s1d	{ DELETE FROM rb WHERE k = 31; }
step s1c	{ COMMIT; }
step s1r	{ ROLLBACK; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2u	{ WITH u AS (UPDATE rb SET v = v + 1 WHERE grp = 1 RETURNING k, v, ver) SELECT count(*), sum(v), sum(ver) FROM u; }
step s2b	{ WITH u AS (UPDATE rb SET bal = bal + 1 WHERE grp = 1 RETURNING k, bal, ver) SELECT count(*), sum(bal), sum(ver) FROM u; }
step s2d	{ WITH d AS (DELETE FROM rb WHERE grp = 1 RETURNING k, v) SELECT count(*), sum(k), sum(v) FROM d; }
step s2x	{ WITH a AS (UPDATE rb SET v = v + 1 WHERE k = 21 RETURNING k), b AS (INSERT INTO rlog (k) SELECT k FROM a RETURNING k) SELECT (SELECT count(*) FROM a), (SELECT count(*) FROM b); }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT k, v, grp, bal, ver FROM rb WHERE k IN (1, 11, 21, 26, 31, 41) ORDER BY k; }
step s3log	{ SELECT k FROM rlog ORDER BY k; }

permutation s1mv s1v s1b s1d s2u s1c s2c s3sel
permutation s1mv s1v s1b s1d s2b s1c s2c s3sel
permutation s1mv s1v s1b s1d s2d s1c s2c s3sel
permutation s1v s1b s2x s1c s2c s3sel s3log
permutation s1mv s1v s2b s1r s2c s3sel

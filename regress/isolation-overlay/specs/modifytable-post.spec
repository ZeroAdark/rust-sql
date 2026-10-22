# D2772 POST (modifytable; regress triggers#500): a data-modifying CTE nothing reads runs once, after the statement's
# own UPDATE / DELETE (PostgreSQL's ExecPostprocessPlan), never inside that write's EvalPlanQual: a recheck of the own
# write reruns none of the CTE's plan. s1 changes, moves out of the qual and deletes rows the own write then rechecks; the
# CTE's INSERT leaves one row per statement in pol (its BEFORE STATEMENT trigger fires once, after the own rows' BEFORE
# ROW ones), and beside the CTE's UPDATE of a partitioned root (triggers#500's form) its BEFORE STATEMENT trigger fires
# after the own leaf's BEFORE ROW ones, each fired once over the rechecked version.

setup
{
 CREATE TABLE pok (k int, v int, note text);
 CREATE TABLE pou (k int, v int, note text) PARTITION BY LIST (k);
 CREATE TABLE pou1 PARTITION OF pou FOR VALUES IN (1);
 CREATE TABLE pou2 PARTITION OF pou FOR VALUES IN (2);
 CREATE TABLE pol (a int);
 CREATE TABLE poq (a int) PARTITION BY LIST (a);
 CREATE TABLE poq1 PARTITION OF poq FOR VALUES IN (1);
 CREATE TABLE poq2 PARTITION OF poq FOR VALUES IN (2);
 CREATE FUNCTION po_row() RETURNS trigger LANGUAGE plpgsql AS $$
 BEGIN
   IF TG_OP = 'DELETE' THEN
     RAISE NOTICE '% % old %', TG_NAME, TG_OP, OLD;
     RETURN OLD;
   END IF;
   RAISE NOTICE '% % old % new %', TG_NAME, TG_OP, OLD, NEW;
   RETURN NEW;
 END $$;
 CREATE FUNCTION po_stmt() RETURNS trigger LANGUAGE plpgsql AS $$
 BEGIN
   RAISE NOTICE '% % %', TG_NAME, TG_WHEN, TG_OP;
   RETURN NULL;
 END $$;
 CREATE TRIGGER pok_bru BEFORE UPDATE ON pok FOR EACH ROW EXECUTE FUNCTION po_row();
 CREATE TRIGGER pok_brd BEFORE DELETE ON pok FOR EACH ROW EXECUTE FUNCTION po_row();
 CREATE TRIGGER pou1_bru BEFORE UPDATE ON pou1 FOR EACH ROW EXECUTE FUNCTION po_row();
 CREATE TRIGGER pol_bs BEFORE INSERT ON pol FOR EACH STATEMENT EXECUTE FUNCTION po_stmt();
 CREATE TRIGGER poq_bs BEFORE UPDATE ON poq FOR EACH STATEMENT EXECUTE FUNCTION po_stmt();
 CREATE TRIGGER poq_as AFTER UPDATE ON poq FOR EACH STATEMENT EXECUTE FUNCTION po_stmt();
 INSERT INTO pok SELECT i, i, 'r' || i FROM generate_series(1, 8) i;
 INSERT INTO pou SELECT 1 + i % 2, i, 'r' || i FROM generate_series(1, 8) i;
 INSERT INTO poq VALUES (1), (2);
 ANALYZE pok;
 ANALYZE pou;
 ANALYZE poq;
}

teardown
{
 DROP TABLE pok, pou, pol, poq;
 DROP FUNCTION po_row();
 DROP FUNCTION po_stmt();
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1u	{ UPDATE pok SET note = note || '*' WHERE v IN (2, 3); }
step s1f	{ UPDATE pok SET v = v + 50 WHERE v = 4; }
step s1d	{ DELETE FROM pok WHERE v = 1; }
step s1p	{ UPDATE pou SET note = note || '*' WHERE v IN (2, 3); }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2d	{ WITH c AS (INSERT INTO pol VALUES (1)) DELETE FROM pok WHERE v < 5; }
step s2u	{ WITH c AS (INSERT INTO pol VALUES (2)) UPDATE pok SET note = 'own' WHERE v < 5; }
step s2t	{ WITH c AS (UPDATE poq SET a = a) UPDATE pou SET note = 'own' WHERE v < 5; }
step s2c	{ COMMIT; }

session s3
step s3k	{ SELECT k, v, note FROM pok ORDER BY k; }
step s3l	{ SELECT count(*), sum(a) FROM pol; }
step s3p	{ SELECT tableoid::regclass AS leaf, k, v, note FROM pou ORDER BY v, k; }

permutation s1u s1f s1d s2d s1c s2c s3k s3l
permutation s1u s1f s1d s2u s1c s2c s3k s3l
permutation s1p s2t s1c s2c s3p

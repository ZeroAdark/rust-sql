# mergecmd D655 Row C (b) (merge/epq.rs PROBED and BEFORE ROW): a MERGE in a data-modifying CTE whose target's own scan
# probes the source's build (a Hash Left Join under BY SOURCE actions, a Hash Join under MATCHED actions alone: the source
# is the smaller side, nestloop / mergejoin off, EXPLAIN pins each plan) rechecks in the IR, as ExecMergeMatched does.
# s2's CTE waits on target rows s1 changed; after s1 commits: a MATCHED row whose key moved no longer joins, so under BY
# SOURCE its BY SOURCE actions run over the newest version (the join condition over it and the source row the plan
# slot holds fails), and under MATCHED actions alone nothing runs (the empty EvalPlanQual: the redo tests the hash
# clause over the source key the step's row carries, here through RETURNING s.id); a MATCHED row whose
# condition flipped takes the other MATCHED action; a BY SOURCE row s1 changed runs the BY SOURCE list again over the
# newest version, with no EvalPlanQual (here its first action, a DELETE, now holds). rpg fires a BEFORE ROW trigger:
# the action's prologue locks the newest version before any trigger runs and the trigger fires once, OLD the newest
# version (NOTICEs for a few ids only). With s1 rolled back the MERGE writes the versions it read.
setup
{
  CREATE TABLE rpt (id int PRIMARY KEY, n int NOT NULL, tag text NOT NULL);
  INSERT INTO rpt SELECT g, g, 'old' FROM generate_series(1, 2000) g;
  CREATE TABLE rps (id int NOT NULL, d int NOT NULL);
  INSERT INTO rps VALUES (4, 1), (5, 1), (6, 1), (7, 1), (8, 1), (9, 1);
  CREATE TABLE rpg (id int PRIMARY KEY, n int NOT NULL, tag text NOT NULL);
  INSERT INTO rpg SELECT g, g, 'old' FROM generate_series(1, 300) g;
  CREATE FUNCTION rpg_tg() RETURNS trigger LANGUAGE plpgsql AS
  $$
  BEGIN
    IF tg_op = 'DELETE' THEN
      IF OLD.id BETWEEN 4 AND 9 OR OLD.id > 1000 THEN RAISE NOTICE 'delete %', OLD; END IF;
      RETURN OLD;
    END IF;
    IF OLD.id BETWEEN 4 AND 9 OR OLD.id > 1000 THEN RAISE NOTICE 'update % -> %', OLD, NEW; END IF;
    RETURN NEW;
  END
  $$;
  CREATE TRIGGER rpg_tg BEFORE UPDATE OR DELETE ON rpg FOR EACH ROW EXECUTE FUNCTION rpg_tg();
}
setup { ANALYZE rpt, rps, rpg; }

teardown
{
  DROP TABLE rpt, rps, rpg;
  DROP FUNCTION rpg_tg();
}

session s1
setup		{ BEGIN; }
step s1k	{ UPDATE rpt SET id = 3006 WHERE id = 6; }
step s1n	{ UPDATE rpt SET n = 900 WHERE id = 5; }
step s1b	{ UPDATE rpt SET n = 1600 WHERE id = 100; }
step s1q	{ UPDATE rpt SET tag = 'new' WHERE id = 7; }
step s1gk	{ UPDATE rpg SET id = 3006 WHERE id = 6; }
step s1gn	{ UPDATE rpg SET n = 900 WHERE id = 5; }
step s1gb	{ UPDATE rpg SET tag = 'new' WHERE id = 100; }
step s1c	{ COMMIT; }
step s1r	{ ROLLBACK; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2xb	{ EXPLAIN (COSTS OFF) WITH m AS (MERGE INTO rpt t USING rps s ON t.id = s.id WHEN MATCHED AND t.n > 500 THEN DELETE WHEN MATCHED THEN UPDATE SET n = t.n + s.d, tag = 'm' WHEN NOT MATCHED BY SOURCE AND t.n > 1500 THEN DELETE WHEN NOT MATCHED BY SOURCE THEN UPDATE SET n = t.n + 1, tag = 'bs' RETURNING merge_action() AS act, t.id, t.n) SELECT act, count(*), sum(n), sum(id) FROM m GROUP BY act ORDER BY act; }
step s2xi	{ EXPLAIN (COSTS OFF) WITH m AS (MERGE INTO rpt t USING rps s ON t.id = s.id WHEN MATCHED AND t.n > 500 THEN DELETE WHEN MATCHED THEN UPDATE SET n = t.n + s.d, tag = t.tag || '+' RETURNING merge_action() AS act, t.id, s.id AS sid, t.n, t.tag) SELECT * FROM m ORDER BY id; }
step s2xg	{ EXPLAIN (COSTS OFF) WITH m AS (MERGE INTO rpg t USING rps s ON t.id = s.id WHEN MATCHED AND t.n > 500 THEN DELETE WHEN MATCHED THEN UPDATE SET n = t.n + s.d, tag = 'm' WHEN NOT MATCHED BY SOURCE THEN UPDATE SET tag = 'bs' RETURNING merge_action() AS act, t.id, t.n) SELECT act, count(*), sum(n), sum(id) FROM m GROUP BY act ORDER BY act; }
step s2b	{ WITH m AS (MERGE INTO rpt t USING rps s ON t.id = s.id WHEN MATCHED AND t.n > 500 THEN DELETE WHEN MATCHED THEN UPDATE SET n = t.n + s.d, tag = 'm' WHEN NOT MATCHED BY SOURCE AND t.n > 1500 THEN DELETE WHEN NOT MATCHED BY SOURCE THEN UPDATE SET n = t.n + 1, tag = 'bs' RETURNING merge_action() AS act, t.id, t.n) SELECT act, count(*), sum(n), sum(id) FROM m GROUP BY act ORDER BY act; }
step s2i	{ WITH m AS (MERGE INTO rpt t USING rps s ON t.id = s.id WHEN MATCHED AND t.n > 500 THEN DELETE WHEN MATCHED THEN UPDATE SET n = t.n + s.d, tag = t.tag || '+' RETURNING merge_action() AS act, t.id, s.id AS sid, t.n, t.tag) SELECT * FROM m ORDER BY id; }
step s2g	{ WITH m AS (MERGE INTO rpg t USING rps s ON t.id = s.id WHEN MATCHED AND t.n > 500 THEN DELETE WHEN MATCHED THEN UPDATE SET n = t.n + s.d, tag = 'm' WHEN NOT MATCHED BY SOURCE THEN UPDATE SET tag = 'bs' RETURNING merge_action() AS act, t.id, t.n) SELECT act, count(*), sum(n), sum(id) FROM m GROUP BY act ORDER BY act; }

session s3
step s3t	{ SELECT id, n, tag FROM rpt WHERE id IN (4, 5, 6, 7, 99, 100, 101, 1501, 3006) ORDER BY id; }
step s3c	{ SELECT tag, count(*), sum(n) FROM rpt GROUP BY tag ORDER BY tag; }
step s3g	{ SELECT id, n, tag FROM rpg WHERE id IN (4, 5, 6, 7, 100, 3006) ORDER BY id; }

permutation s2xb s2xi s2xg s1k s1n s1b s1q s2b s1c s3t s3c
permutation s1k s1n s1q s2i s1c s3t
permutation s1gk s1gn s1gb s2g s1c s3g
permutation s1k s1n s1b s2b s1r s3t s3c

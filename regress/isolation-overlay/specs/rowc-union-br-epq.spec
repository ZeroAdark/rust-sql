# D3373 (5) (modifytable_rowc_union :88): a data-modifying CTE's DELETE / UPDATE of a hash-partitioned table, one leaf with a
# BEFORE ROW trigger. The CTE's write reads one settled union of the leaves' scans (ROWC-UNION). Over a newer version of a
# row of that leaf (READ COMMITTED) PostgreSQL's EvalPlanQual runs in GetTupleForTrigger: it locks the newest version, reruns
# that leaf's scan quals over it, and only then fires the trigger (OLD the locked version); a row s1 deleted or moved out of
# the WHERE is skipped and fires nothing. The trigger logs each row it sees (OLD.v: the version it locked). The UPDATE's SET
# list reads a plain column (an arm that can raise keeps the union refused, D1486). Expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE rb (id int NOT NULL, k int, v int) PARTITION BY HASH (id);
  CREATE TABLE rb0 PARTITION OF rb FOR VALUES WITH (MODULUS 2, REMAINDER 0);
  CREATE TABLE rb1 PARTITION OF rb FOR VALUES WITH (MODULUS 2, REMAINDER 1);
  INSERT INTO rb SELECT i, i % 10, i * 3 FROM generate_series(1, 200) i;
  CREATE TABLE rblog (id int, v int, op text);
  CREATE FUNCTION rb_trg() RETURNS trigger LANGUAGE plpgsql AS $$
  BEGIN
    IF TG_OP = 'DELETE' THEN INSERT INTO rblog VALUES (OLD.id, OLD.v, TG_OP); RETURN OLD; END IF;
    INSERT INTO rblog VALUES (NEW.id, OLD.v, TG_OP); NEW.v := NEW.v + 1; RETURN NEW;
  END $$;
  CREATE TRIGGER rb_bd BEFORE DELETE OR UPDATE ON rb1 FOR EACH ROW EXECUTE FUNCTION rb_trg();
  ANALYZE rb;
}
teardown { DROP TABLE rb, rblog; DROP FUNCTION rb_trg(); }

session s1
setup		{ BEGIN; }
step s1u	{ UPDATE rb SET v = v + 1000 WHERE id IN (1, 2, 11, 12); }
step s1k	{ UPDATE rb SET k = 9 WHERE id IN (21, 22); }
step s1d	{ DELETE FROM rb WHERE id IN (31, 32); }
step s1c	{ COMMIT; }

session s2
setup		{ SET max_parallel_workers_per_gather = 0; }
step s2d	{ WITH d AS (DELETE FROM rb WHERE k IN (1, 2) RETURNING id, v) SELECT count(*), sum(v) FROM d; }
step s2u	{ WITH u AS (UPDATE rb SET v = k WHERE k IN (1, 2) RETURNING id, v) SELECT count(*), sum(v) FROM u; }
step s2r	{ SELECT id, v, op FROM rblog ORDER BY id, op; }
step s2t	{ SELECT tableoid::regclass, count(*), sum(v) FROM rb GROUP BY 1 ORDER BY 1; }

permutation s1u s2d s1c s2r s2t
permutation s1k s1d s2d s1c s2r s2t
permutation s1u s2u s1c s2r s2t
permutation s1k s1d s2u s1c s2r s2t

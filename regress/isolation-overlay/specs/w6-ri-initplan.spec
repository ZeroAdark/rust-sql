# W6-D652-LOCK-INITPLAN (V4R-527, crates/gravity/lowerv4/src/lock_init.rs): a row lock's EvalPlanQual beside an InitPlan.
# The temporal NO ACTION restrict check (ri_triggers.c ri_restrict) is LockRows(x) over a filter reading InitPlan 1, whose
# Aggregate reads LockRows(y); EvalPlanQualStart copies InitPlan 1's value and never reruns it (x's recheck never calls
# the VOLATILE w6r_bump of its scan again), and y's own recheck runs inside it (calling it again for the row it rechecks).
# - the RI trigger: s1 changes a referencing row x's key and holds it; s2's DELETE / UPDATE of y waits in its check, then
#   rechecks x's newest version (still referencing: the error; moved out of the changed range: the UPDATE succeeds);
# - the check's own statement s2q, READ COMMITTED: s1 changes the key of the one x row reaching the lock (kept, filtered
#   out by the recheck, deleted, rolled back) or of a y row the InitPlan locks (its recheck calls w6r_bump again), holds
#   it; s2q waits, then rechecks the newest version against the InitPlan's one value;
# - REPEATABLE READ / SERIALIZABLE: s1's committed key change of that x or y row after s2's snapshot: 40001.
# V4R-579 (W6-D652-R2): the same check over a PARTITIONED referencing table w6fp (a leaf in another list, a sub-partitioned
#   leaf): the check's lock (B) over w6fp's leaves beside the InitPlan's lock of w6rp. s1 changes the key of a leaf-2 row
#   (s2's check waits on it, rechecks it and still finds it: the error), moves it to leaf 1 (its new row's own FK check
#   locks the PK row first: s2's DELETE waits there, then its check finds the moved row), moves its period off the deleted
#   one or deletes it (the check's recheck drops it: the DELETE succeeds), changes the key or the period of the
#   sub-partitioned leaf's row; s2qp is the check's own statement form over the leaves (kept with its new key, filtered,
#   deleted, moved: 40001 already moved to another partition); REPEATABLE READ: 40001.
# Expected output is the C oracle's (PostgreSQL 18.6, pgdg).

setup
{
  CREATE SEQUENCE w6r_seq;
  CREATE FUNCTION w6r_bump(int4range) RETURNS boolean LANGUAGE plpgsql VOLATILE AS $$ BEGIN PERFORM nextval('w6r_seq'); RETURN true; END $$;
  CREATE TABLE w6r (id int4range, valid_at daterange, CONSTRAINT w6r_pk PRIMARY KEY (id, valid_at WITHOUT OVERLAPS));
  CREATE TABLE w6f (id int4range, valid_at daterange, parent_id int4range,
    CONSTRAINT w6f_pk PRIMARY KEY (id, valid_at WITHOUT OVERLAPS),
    CONSTRAINT w6f_fk FOREIGN KEY (parent_id, PERIOD valid_at) REFERENCES w6r (id, PERIOD valid_at));
  INSERT INTO w6r VALUES ('[5,6)', daterange('2018-01-01', '2018-02-01')), ('[5,6)', daterange('2018-02-01', '2018-03-01'));
  INSERT INTO w6f VALUES ('[3,4)', daterange('2018-01-05', '2018-01-10'), '[5,6)'), ('[4,5)', daterange('2018-01-20', '2018-02-10'), '[5,6)');
  CREATE TABLE w6rp (id int4range, valid_at daterange, CONSTRAINT w6rp_pk PRIMARY KEY (id, valid_at WITHOUT OVERLAPS));
  CREATE TABLE w6fp (id int4range, valid_at daterange, parent_id int4range,
    CONSTRAINT w6fp_pk PRIMARY KEY (id, valid_at WITHOUT OVERLAPS),
    CONSTRAINT w6fp_fk FOREIGN KEY (parent_id, PERIOD valid_at) REFERENCES w6rp (id, PERIOD valid_at)) PARTITION BY LIST (id);
  CREATE TABLE w6fp1 PARTITION OF w6fp FOR VALUES IN ('[1,2)', '[3,4)', '[7,8)');
  CREATE TABLE w6fp2 PARTITION OF w6fp FOR VALUES IN ('[2,3)', '[4,5)', '[8,9)');
  CREATE TABLE w6fp3 PARTITION OF w6fp FOR VALUES IN ('[5,6)', '[6,7)', '[10,11)') PARTITION BY LIST (id);
  CREATE TABLE w6fp3a PARTITION OF w6fp3 FOR VALUES IN ('[5,6)');
  CREATE TABLE w6fp3b PARTITION OF w6fp3 FOR VALUES IN ('[6,7)', '[10,11)');
  INSERT INTO w6rp VALUES ('[5,6)', daterange('2018-01-01', '2018-02-01')), ('[5,6)', daterange('2018-02-01', '2018-03-01')), ('[9,10)', daterange('2018-01-01', '2018-03-01'));
  INSERT INTO w6fp VALUES ('[3,4)', daterange('2018-02-05', '2018-02-10'), '[5,6)'), ('[4,5)', daterange('2018-01-20', '2018-01-25'), '[5,6)'), ('[6,7)', daterange('2018-01-22', '2018-01-25'), '[9,10)');
}

teardown
{
  DROP TABLE w6f;
  DROP TABLE w6r;
  DROP TABLE w6fp;
  DROP TABLE w6rp;
  DROP FUNCTION w6r_bump(int4range);
  DROP SEQUENCE w6r_seq;
}

session s1
setup		{ BEGIN; }
step s1kx	{ UPDATE w6f SET id = '[7,8)' WHERE id = '[3,4)'; }
step s1kz	{ UPDATE w6f SET id = '[8,9)' WHERE id = '[4,5)'; }
step s1mz	{ UPDATE w6f SET valid_at = daterange('2018-01-12', '2018-01-15') WHERE id = '[4,5)'; }
step s1dz	{ DELETE FROM w6f WHERE id = '[4,5)'; }
step s1ey	{ UPDATE w6r SET valid_at = daterange('2017-12-01', '2018-02-01') WHERE valid_at = daterange('2018-01-01', '2018-02-01'); }
step s1pk	{ UPDATE w6fp SET id = '[8,9)' WHERE id = '[4,5)'; }
step s1pm	{ UPDATE w6fp SET id = '[7,8)' WHERE id = '[4,5)'; }
step s1pv	{ UPDATE w6fp SET valid_at = daterange('2018-02-12', '2018-02-15') WHERE id = '[4,5)'; }
step s1pd	{ DELETE FROM w6fp WHERE id = '[4,5)'; }
step s1sv	{ UPDATE w6fp SET valid_at = daterange('2018-01-22', '2018-01-26') WHERE id = '[6,7)'; }
step s1sk	{ UPDATE w6fp SET id = '[10,11)' WHERE id = '[6,7)'; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
step s2dy	{ DELETE FROM w6r WHERE valid_at = daterange('2018-01-01', '2018-02-01'); }
step s2uy	{ UPDATE w6r SET valid_at = daterange('2018-02-15', '2018-03-01') WHERE valid_at = daterange('2018-02-01', '2018-03-01'); }
step s2q	{ SELECT x.id FROM ONLY w6f x WHERE x.parent_id = '[5,6)' AND NOT coalesce((x.valid_at * daterange('2018-01-01', '2018-03-01')) <@ (SELECT range_agg(r) FROM (SELECT y.valid_at r FROM ONLY w6r y WHERE y.id = '[5,6)' AND y.valid_at && daterange('2018-01-01', '2018-02-01') AND w6r_bump(y.id) FOR KEY SHARE OF y) y2), false) FOR KEY SHARE OF x; }
step s2n	{ SELECT CASE WHEN is_called THEN last_value ELSE 0 END AS calls FROM w6r_seq; }
step s2rr	{ BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM w6f; }
step s2sr	{ BEGIN ISOLATION LEVEL SERIALIZABLE; SELECT count(*) FROM w6r; }
step s2c	{ COMMIT; }
step s2r	{ SELECT * FROM w6r ORDER BY valid_at; SELECT * FROM w6f ORDER BY id; }
step s2dp	{ DELETE FROM w6rp WHERE id = '[5,6)' AND valid_at = daterange('2018-01-01', '2018-02-01'); }
step s2dq	{ DELETE FROM w6rp WHERE id = '[9,10)'; }
step s2qp	{ SELECT x.id, x.valid_at FROM w6fp x WHERE x.parent_id = '[5,6)' AND NOT coalesce((x.valid_at * daterange('2018-01-01', '2018-03-01')) <@ (SELECT range_agg(r) FROM (SELECT y.valid_at r FROM ONLY w6rp y WHERE y.id = '[5,6)' AND y.valid_at && daterange('2018-02-01', '2018-03-01') FOR KEY SHARE OF y) y2), false) FOR KEY SHARE OF x; }
step s2rp	{ BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM w6fp; }
step s2pr	{ SELECT * FROM w6rp ORDER BY id, valid_at; SELECT tableoid::regclass, * FROM w6fp ORDER BY id; }

permutation s1kx s2dy s1c s2r
permutation s1mz s2uy s1c s2r
permutation s1kz s2q s1c s2n
permutation s1mz s2q s1c s2n
permutation s1dz s2q s1c s2n
permutation s1kz s2q s1a s2n
permutation s1ey s2q s1c s2n
permutation s2rr s1kz s1c s2q s2c s2n
permutation s2sr s1ey s1c s2q s2c s2n
permutation s1pk s2dp s1c s2pr
permutation s1pm s2dp s1c s2pr
permutation s1pv s2dp s1c s2pr
permutation s1pd s2dp s1c s2pr
permutation s1sv s2dq s1c s2pr
permutation s1sk s2dq s1c s2pr
permutation s1pk s2qp s1c
permutation s1pv s2qp s1c
permutation s1pd s2qp s1c
permutation s1pm s2qp s1c
permutation s2rp s1pk s1c s2dp s2c s2pr

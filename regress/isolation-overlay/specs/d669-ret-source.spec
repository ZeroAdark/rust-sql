# D669 (M4-CLOSE B3c): UPDATE .. FROM, DELETE .. USING and MERGE .. RETURNING a source column across an EvalPlanQual
# recheck. PostgreSQL's ExecUpdate / ExecDelete / ExecMergeMatched project RETURNING over the plan slot the statement
# fetched (context->planSlot), whose source columns are the version its snapshot joined; the recheck re-joins the new
# target version with the source row its mark saved (REFERENCE: the same version by ctid; COPY: the copied row). Cases:
# a concurrent UPDATE of the target row (still joined, or moved out of the join), of the target and the source row
# together (the source's new values must not appear), a VALUES source (a COPY mark), and MERGE's UPDATE / DELETE actions.
# The writer session plans hash joins (enable_nestloop / enable_mergejoin off): Gravity serves those at M4.
setup
{
 CREATE TABLE t669 (id int PRIMARY KEY, v int NOT NULL, note text);
 INSERT INTO t669 VALUES (1, 100, 'a'), (2, 200, 'b'), (3, 300, 'c'), (4, 400, 'd');
 CREATE TABLE s669 (id int PRIMARY KEY, delta int NOT NULL, tag text);
 INSERT INTO s669 VALUES (1, 10, 's1'), (2, 20, 's2'), (3, 30, 's3'), (4, 40, 's4');
}
teardown { DROP TABLE t669; DROP TABLE s669; }
session s1
setup { BEGIN ISOLATION LEVEL READ COMMITTED; SET enable_nestloop = off; SET enable_mergejoin = off; }
step uf { UPDATE t669 t SET v = t.v + s.delta FROM s669 s WHERE t.id = s.id AND s.id IN (1, 2) RETURNING t.id, t.v, s.delta, s.tag, t.note; }
step uv { UPDATE t669 t SET v = t.v + x.d FROM (VALUES (1, 5, 'x1'), (2, 6, 'x2')) x(id, d, tag) WHERE t.id = x.id RETURNING t.id, t.v, x.d, x.tag; }
step du { DELETE FROM t669 t USING s669 s WHERE t.id = s.id AND s.id IN (1, 2) RETURNING t.id, t.v, s.delta, s.tag; }
step mu { MERGE INTO t669 t USING s669 s ON t.id = s.id WHEN MATCHED AND t.id IN (1, 2) THEN UPDATE SET v = t.v + s.delta RETURNING merge_action(), t.id, t.v, s.delta, s.tag; }
step md { MERGE INTO t669 t USING s669 s ON t.id = s.id WHEN MATCHED AND t.id IN (1, 2) THEN DELETE RETURNING merge_action(), t.id, t.v, s.tag; }
step c1 { COMMIT; }
session s2
setup { BEGIN ISOLATION LEVEL READ COMMITTED; }
step wt { UPDATE t669 SET v = v + 1000, note = 'a2' WHERE id = 1; }
step wts { UPDATE t669 SET v = v + 1000 WHERE id = 1; UPDATE s669 SET delta = delta + 7, tag = 's1new' WHERE id = 1; }
step wtm { UPDATE t669 SET id = 11 WHERE id = 1; }
step c2 { COMMIT; }
session s3
step read { SELECT * FROM t669 ORDER BY id; }
permutation wt uf c2 c1 read
permutation wts uf c2 c1 read
permutation wtm uf c2 c1 read
permutation wt uv c2 c1 read
permutation wts uv c2 c1 read
permutation wt du c2 c1 read
permutation wts du c2 c1 read
permutation wtm du c2 c1 read
permutation wt mu c2 c1 read
permutation wts mu c2 c1 read
permutation wtm mu c2 c1 read
permutation wts md c2 c1 read

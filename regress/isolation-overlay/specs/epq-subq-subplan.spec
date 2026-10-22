# D1478 (M8-SUB): a SubPlan or InitPlan below LockRows or a write, under the EvalPlanQual recheck. PostgreSQL's
# PlanRowMarks name the top-level query's relations alone: the rechecked row is the locked relation's newest version, an
# unlocked joined relation (a REFERENCE mark, a pulled-up subquery's too) keeps the row the join read, and a SubPlan's
# or an InitPlan's own scan runs again under the statement's snapshot (an InitPlan's value is the one computed before).
# One step per form: a by-reference record, a by-value int, a by-reference text, EXISTS, ARRAY, a SubPlan over the
# locked relation itself (its length: the snapshot's version), an InitPlan, a FOR UPDATE OF join partner beside a
# SubPlan, a flattened subquery's join, an UPDATE whose SET reads a SubPlan, and an UPDATE ... FROM a flattened subquery
# (its REFERENCE-marked row kept) whose SET reads a SubPlan. The joins are hashed (s2 turns nested
# loops and merge joins off). Expected output is the C oracle's (PostgreSQL 18.6).

setup
{
 CREATE TABLE table_a (id integer, value text);
 CREATE TABLE table_b (id integer, value text);
 INSERT INTO table_a VALUES (1, 'tableAValue');
 INSERT INTO table_b VALUES (1, 'tableBValue');
}

teardown
{
 DROP TABLE table_a, table_b;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step upd	{
	UPDATE table_a SET value = 'newTableAValue' WHERE id = 1;
	UPDATE table_b SET value = 'newTableBValue' WHERE id = 1;
}
step c1		{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL enable_nestloop = off; SET LOCAL enable_mergejoin = off; }
step rowv	{ SELECT ta.id, ta.value, (SELECT ROW(tb.id, tb.value) FROM table_b tb WHERE ta.id = tb.id) AS tb_row FROM table_a ta WHERE ta.id = 1 FOR UPDATE; }
step intv	{ SELECT ta.id, ta.value, (SELECT tb.id + 10 FROM table_b tb WHERE ta.id = tb.id) AS tb_id FROM table_a ta WHERE ta.id = 1 FOR UPDATE; }
step txtv	{ SELECT ta.id, ta.value, (SELECT tb.value FROM table_b tb WHERE ta.id = tb.id) AS tb_value FROM table_a ta WHERE ta.id = 1 FOR UPDATE; }
step exv	{ SELECT ta.id, ta.value, EXISTS (SELECT 1 FROM table_b tb WHERE ta.id = tb.id) AS tb_exists FROM table_a ta WHERE ta.id = 1 FOR UPDATE; }
step arrv	{ SELECT ta.id, ta.value, ARRAY(SELECT tb.value FROM table_b tb WHERE ta.id = tb.id) AS tb_values FROM table_a ta WHERE ta.id = 1 FOR UPDATE; }
step selfv	{ SELECT ta.id, ta.value, (SELECT length(ta2.value) FROM table_a ta2 WHERE ta2.id = ta.id) AS ta_seen_len FROM table_a ta WHERE ta.id = 1 FOR UPDATE; }
step initv	{ SELECT ta.id, ta.value, (SELECT max(tb.value) FROM table_b tb) AS tb_max FROM table_a ta WHERE ta.id = 1 FOR UPDATE; }
step joinv	{ SELECT ta.id, ta.value, tb.value AS tb_value, (SELECT tb2.id + 10 FROM table_b tb2 WHERE tb2.id = ta.id) AS tb_id FROM table_a ta JOIN table_b tb ON tb.id = ta.id WHERE ta.id = 1 FOR UPDATE OF ta; }
step pullv	{ SELECT ta.id, ta.value, s.value AS s_value FROM table_a ta JOIN (SELECT id, value FROM table_b) s ON s.id = ta.id WHERE ta.id = 1 FOR UPDATE OF ta; }
step updv	{ UPDATE table_a ta SET value = ta.value || '/' || (SELECT length(tb.value) FROM table_b tb WHERE tb.id = ta.id) WHERE ta.id = 1 RETURNING ta.id, ta.value; }
step updjv	{ UPDATE table_a ta SET value = ta.value || '/' || tb.value || '/' || (SELECT length(tb2.value) FROM table_b tb2 WHERE tb2.id = ta.id) FROM (SELECT id, value FROM table_b) tb WHERE tb.id = ta.id AND ta.id = 1 RETURNING ta.id, ta.value; }
step c2		{ COMMIT; }

permutation upd rowv c1 c2
permutation upd intv c1 c2
permutation upd txtv c1 c2
permutation upd exv c1 c2
permutation upd arrv c1 c2
permutation upd selfv c1 c2
permutation upd initv c1 c2
permutation upd joinv c1 c2
permutation upd pullv c1 c2
permutation upd updv c1 c2
permutation upd updjv c1 c2

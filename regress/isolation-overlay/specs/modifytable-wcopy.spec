# WCOPY (modifytable): an UPDATE / DELETE whose target is the parameterized inner of a nested loop over a subquery row
# (ROW_MARK_COPY: a Subquery Scan over a WindowAgg) meets a target row s1 changed. EvalPlanQual hands the Subquery Scan
# the row it stored and the target's index scan the newest version, its key checked again against that row: a row that
# still matches is written over its newest version, one s1 moved off the key or deleted is skipped, and s1's change to
# the window's input is not seen (the window never runs again: the running sums stay the snapshot's).

setup
{
 CREATE TABLE wcn_o (order_id int PRIMARY KEY, customer_id int, qty int NOT NULL, points bigint NOT NULL, note text);
 INSERT INTO wcn_o SELECT i, i % 30, 1 + (i * 31) % 9, 0, 'o' || i FROM generate_series(1, 300) i;
 CREATE INDEX wcn_o_cust ON wcn_o (customer_id, order_id);
 ANALYZE wcn_o;
}

teardown
{
 DROP TABLE wcn_o;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1u	{ UPDATE wcn_o SET note = note || '*' WHERE order_id IN (9, 39); }
step s1k	{ UPDATE wcn_o SET order_id = order_id + 1000 WHERE order_id = 69; }
step s1d	{ DELETE FROM wcn_o WHERE order_id = 99; }
step s1o	{ UPDATE wcn_o SET qty = qty + 100, note = note || '+' WHERE order_id = 129; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2u	{ UPDATE wcn_o o SET points = s.running FROM (SELECT order_id, sum(qty) OVER (PARTITION BY customer_id ORDER BY order_id) AS running FROM wcn_o WHERE customer_id = 9) s WHERE s.order_id = o.order_id RETURNING o.order_id, o.qty, o.points, o.note; }
step s2d	{ DELETE FROM wcn_o o USING (SELECT order_id, row_number() OVER (ORDER BY order_id) AS rn FROM wcn_o WHERE customer_id = 9) s WHERE s.order_id = o.order_id AND s.rn <= 6; }
step s2c	{ COMMIT; }

session s3
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; }
step s3s	{ SELECT order_id, customer_id, qty, points, note FROM wcn_o WHERE customer_id = 9 OR order_id > 1000 ORDER BY order_id; }
step s3e	{ EXPLAIN (COSTS OFF) UPDATE wcn_o o SET points = s.running FROM (SELECT order_id, sum(qty) OVER (PARTITION BY customer_id ORDER BY order_id) AS running FROM wcn_o WHERE customer_id = 9) s WHERE s.order_id = o.order_id RETURNING o.order_id, o.qty, o.points, o.note; }

permutation s3e s1u s1k s1d s1o s2u s1c s2c s3s
permutation s1u s1k s1d s1o s2d s1c s2c s3s

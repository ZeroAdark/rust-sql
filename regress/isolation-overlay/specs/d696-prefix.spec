# D696: a multi-row UPDATE by a prefix of a unique index (3 of order_line's 4 key columns, the 10 lines of order
# (1, 3, 7)) waits on a line a concurrent transaction updated, rechecks it after that transaction ends (EvalPlanQual)
# and goes on to the lines after it: UPDATE 10 (9 when the concurrent update moved the line out of the qual). The shape
# of e2e m4_oltp's 'range-write gate, UPDATE by a prefix of a unique index', where trunk d55e09586b8's backend died
# (SIGSEGV) after the lock wait. order_line as bench/oltp/tpcc-schema.sql declares it; with these stats (6,000 rows,
# ANALYZE) the prefix is a Bitmap Heap Scan over order_line_i1 (PostgreSQL 18.6 and pgrust alike); session ai runs the
# same UPDATE with enable_bitmapscan = off, a plain Index Scan. Permutation pa pai pins both plans.
setup
{
 CREATE TABLE order_line (ol_delivery_d timestamp with time zone, ol_o_id integer NOT NULL, ol_w_id integer NOT NULL,
   ol_i_id integer NOT NULL, ol_supply_w_id integer NOT NULL, ol_d_id smallint NOT NULL, ol_number smallint NOT NULL,
   ol_quantity smallint NOT NULL, ol_amount numeric(6,2), ol_dist_info character(24),
   CONSTRAINT order_line_i1 PRIMARY KEY (ol_w_id, ol_d_id, ol_o_id, ol_number));
 INSERT INTO order_line
 SELECT NULL, o, w, (o * 131 + d * 7919 + ln * 524287) % 100000 + 1, w, d, ln, 5, ((o * ln) % 999900 + 1) / 100.0,
        rpad('oldist-' || d || '-' || ln, 24, 'k')
 FROM generate_series(1, 2) w, generate_series(1, 10) d, generate_series(1, 30) o, generate_series(1, 10) ln;
 ANALYZE order_line;
}
teardown { DROP TABLE order_line; }

session b
setup { BEGIN ISOLATION LEVEL READ COMMITTED; }
step b2  { UPDATE order_line SET ol_quantity = ol_quantity + 100 WHERE ol_w_id = 1 AND ol_d_id = 3 AND ol_o_id = 7 AND ol_number = 2; }
step b1  { UPDATE order_line SET ol_quantity = ol_quantity + 100 WHERE ol_w_id = 1 AND ol_d_id = 3 AND ol_o_id = 7 AND ol_number = 1; }
step b10 { UPDATE order_line SET ol_quantity = ol_quantity + 100 WHERE ol_w_id = 1 AND ol_d_id = 3 AND ol_o_id = 7 AND ol_number = 10; }
step bx  { UPDATE order_line SET ol_o_id = 31 WHERE ol_w_id = 1 AND ol_d_id = 3 AND ol_o_id = 7 AND ol_number = 2; }
step bc  { COMMIT; }
step br  { ROLLBACK; }

session a
step pa  { EXPLAIN (COSTS OFF) UPDATE order_line SET ol_quantity = ol_quantity + 1 WHERE ol_w_id = 1 AND ol_d_id = 3 AND ol_o_id = 7; }
step a   { UPDATE order_line SET ol_quantity = ol_quantity + 1 WHERE ol_w_id = 1 AND ol_d_id = 3 AND ol_o_id = 7; }

session ai
setup { SET enable_bitmapscan = off; }
step pai { EXPLAIN (COSTS OFF) UPDATE order_line SET ol_quantity = ol_quantity + 1 WHERE ol_w_id = 1 AND ol_d_id = 3 AND ol_o_id = 7; }
step ai  { UPDATE order_line SET ol_quantity = ol_quantity + 1 WHERE ol_w_id = 1 AND ol_d_id = 3 AND ol_o_id = 7; }

session r
step read { SELECT ol_o_id, ol_number, ol_quantity FROM order_line WHERE ol_w_id = 1 AND ol_d_id = 3 AND ol_o_id IN (7, 31) ORDER BY ol_o_id, ol_number; }

permutation pa pai
permutation b2 a bc read
permutation b2 a br read
permutation b1 a bc read
permutation b10 a bc read
permutation bx a bc read
permutation b2 ai bc read
permutation b1 ai bc read
permutation b10 ai bc read
permutation bx ai bc read

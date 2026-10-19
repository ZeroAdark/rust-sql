-- postgres_fdw batch-98: batch_size read through parse_int, direct-modify
-- RETURNING rows converted one at a time, and a failed async FETCH drained
-- on another Append's behalf reporting its own remote error. Loopback
-- servers on this backend (two of them: distinct connections).
CREATE EXTENSION postgres_fdw;
DO $d$
BEGIN
  EXECUTE format('CREATE SERVER b98_lb FOREIGN DATA WRAPPER postgres_fdw OPTIONS (dbname %L, port %L, host %L)',
                 current_database(), current_setting('port'), current_setting('unix_socket_directories'));
  EXECUTE format('CREATE SERVER b98_lba FOREIGN DATA WRAPPER postgres_fdw OPTIONS (dbname %L, port %L, host %L, async_capable ''true'', fetch_size ''1'')',
                 current_database(), current_setting('port'), current_setting('unix_socket_directories'));
  EXECUTE format('CREATE SERVER b98_lbb FOREIGN DATA WRAPPER postgres_fdw OPTIONS (dbname %L, port %L, host %L, async_capable ''true'', fetch_size ''1'')',
                 current_database(), current_setting('port'), current_setting('unix_socket_directories'));
  EXECUTE format('CREATE USER MAPPING FOR CURRENT_USER SERVER b98_lb OPTIONS (user %L)', current_user);
  EXECUTE format('CREATE USER MAPPING FOR CURRENT_USER SERVER b98_lba OPTIONS (user %L)', current_user);
  EXECUTE format('CREATE USER MAPPING FOR CURRENT_USER SERVER b98_lbb OPTIONS (user %L)', current_user);
END
$d$;
-- batch_size goes through parse_int: octal, hex, whitespace, rounded float
CREATE TABLE b98_bs (a int);
CREATE FOREIGN TABLE b98_ft_bs1 (a int) SERVER b98_lb OPTIONS (table_name 'b98_bs', batch_size '010');
CREATE FOREIGN TABLE b98_ft_bs2 (a int) SERVER b98_lb OPTIONS (table_name 'b98_bs', batch_size '0x10');
CREATE FOREIGN TABLE b98_ft_bs3 (a int) SERVER b98_lb OPTIONS (table_name 'b98_bs', batch_size ' 16.6 ');
EXPLAIN (VERBOSE, COSTS OFF) INSERT INTO b98_ft_bs1 VALUES (1), (2);
EXPLAIN (VERBOSE, COSTS OFF) INSERT INTO b98_ft_bs2 VALUES (1), (2);
EXPLAIN (VERBOSE, COSTS OFF) INSERT INTO b98_ft_bs3 VALUES (1), (2);
INSERT INTO b98_ft_bs1 SELECT g FROM generate_series(1, 20) g;
SELECT count(*) FROM b98_bs;
-- direct DELETE ... RETURNING: the first row's local projection error wins
-- over a conversion error in a later remote row
CREATE TABLE b98_ret (a text);
INSERT INTO b98_ret VALUES ('1'), ('bad');
CREATE FOREIGN TABLE b98_ft_ret (a integer) SERVER b98_lb OPTIONS (table_name 'b98_ret');
EXPLAIN (VERBOSE, COSTS OFF) DELETE FROM b98_ft_ret RETURNING a / (a - a);
DELETE FROM b98_ft_ret RETURNING a / (a - a);
DELETE FROM b98_ft_ret RETURNING a;
SELECT * FROM b98_ret;
-- an in-flight async FETCH that fails, drained by a scan of another Append
-- on the same connection: the FETCH's own error, not the next command's
-- "current transaction is aborted"
CREATE TABLE b98_src (a int);
INSERT INTO b98_src VALUES (1), (2), (3);
CREATE TABLE b98_ok (a int, b int);
INSERT INTO b98_ok VALUES (1, 1), (2, 2), (3, 3);
CREATE VIEW b98_slowfail AS SELECT a, CASE WHEN pg_sleep(1)::text = '' THEN 1 / (a - a) END AS b FROM b98_src;
CREATE TABLE b98_pa (a int, b int) PARTITION BY RANGE (a);
CREATE FOREIGN TABLE b98_pa1 PARTITION OF b98_pa FOR VALUES FROM (0) TO (10) SERVER b98_lba OPTIONS (table_name 'b98_slowfail');
CREATE FOREIGN TABLE b98_pa2 PARTITION OF b98_pa FOR VALUES FROM (10) TO (20) SERVER b98_lbb OPTIONS (table_name 'b98_ok');
CREATE TABLE b98_pb (a int, b int) PARTITION BY RANGE (a);
CREATE FOREIGN TABLE b98_pb1 PARTITION OF b98_pb FOR VALUES FROM (0) TO (10) SERVER b98_lba OPTIONS (table_name 'b98_ok');
CREATE FOREIGN TABLE b98_pb2 PARTITION OF b98_pb FOR VALUES FROM (10) TO (20) SERVER b98_lba OPTIONS (table_name 'b98_ok');
SET enable_hashjoin = off;
SET enable_mergejoin = off;
SET enable_material = off;
EXPLAIN (COSTS OFF) SELECT * FROM b98_pa t1, b98_pb t2 WHERE t1.b = t2.b;
SELECT * FROM b98_pa t1, b98_pb t2 WHERE t1.b = t2.b;
RESET enable_hashjoin;
RESET enable_mergejoin;
RESET enable_material;
DROP TABLE b98_pa, b98_pb;
DROP SERVER b98_lb CASCADE;
DROP SERVER b98_lba CASCADE;
DROP SERVER b98_lbb CASCADE;
DROP VIEW b98_slowfail;
DROP TABLE b98_bs, b98_ret, b98_src, b98_ok;
DROP EXTENSION postgres_fdw;

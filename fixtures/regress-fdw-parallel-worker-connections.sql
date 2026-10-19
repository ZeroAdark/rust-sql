-- regression: postgres_fdw connections opened in parallel workers close when
-- the worker's task ends, like a C worker process exit.
CREATE EXTENSION postgres_fdw;
CREATE TABLE w (a int);
INSERT INTO w SELECT generate_series(1, 20000);
ALTER TABLE w SET (parallel_workers = 8);
ANALYZE w;
CREATE TABLE src (v int);
INSERT INTO src VALUES (1);
SELECT format('CREATE SERVER loop%s FOREIGN DATA WRAPPER postgres_fdw OPTIONS (host %L, port %L, dbname %L, application_name %L)', g, '127.0.0.1', current_setting('port'), 'postgres', 'fdw_loop' || g) FROM generate_series(1, 6) g \gexec
SELECT format('CREATE USER MAPPING FOR PUBLIC SERVER loop%s OPTIONS (user %L)', g, 'postgres') FROM generate_series(1, 6) g \gexec
SELECT format('CREATE FOREIGN TABLE ft%s (v int) SERVER loop%s OPTIONS (table_name %L)', g, g, 'src') FROM generate_series(1, 6) g \gexec
CREATE FUNCTION readall(x int) RETURNS int LANGUAGE sql STABLE PARALLEL SAFE AS $$
  SELECT (SELECT count(*) FROM ft1)::int + (SELECT count(*) FROM ft2)::int + (SELECT count(*) FROM ft3)::int
       + (SELECT count(*) FROM ft4)::int + (SELECT count(*) FROM ft5)::int + (SELECT count(*) FROM ft6)::int
$$;
-- control: a serial session's connections close when it disconnects
SELECT readall(0) AS serial_read;
\c
SELECT pg_sleep(2);
SELECT count(*) AS open_after_serial_session FROM pg_stat_activity WHERE application_name LIKE 'fdw_loop%';
-- parallel session: the workers open the connections
SET max_parallel_workers_per_gather = 8;
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
SET min_parallel_table_scan_size = 0;
SET parallel_leader_participation = off;
SELECT sum(readall(a)) AS parallel_read FROM w WHERE a % 1000 = 0;
SELECT sum(readall(a)) AS parallel_read FROM w WHERE a % 1000 = 0;
\c
SELECT pg_sleep(5);
SELECT count(*) > 0 AS fdw_connections_still_open, count(*) >= 6 AS at_least_one_per_server
FROM pg_stat_activity WHERE application_name LIKE 'fdw_loop%';

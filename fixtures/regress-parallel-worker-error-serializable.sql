\set VERBOSITY verbose
CREATE TABLE ssi_src AS SELECT g AS id, g::text || 'x' AS s FROM generate_series(1, 1000) g;
ANALYZE ssi_src;
CREATE TABLE ssi_log (id int, note text);
-- 1: a parallel worker raises an error; the transaction recovers through a savepoint, then writes
BEGIN ISOLATION LEVEL SERIALIZABLE;
SAVEPOINT s;
SET LOCAL debug_parallel_query = on;
SELECT s::int2 FROM ssi_src WHERE id = 1;
ROLLBACK TO SAVEPOINT s;
INSERT INTO ssi_log VALUES (1, 'after worker error');
COMMIT;
-- 2: the same error raised by the leader (no worker): the INSERT succeeds everywhere
BEGIN ISOLATION LEVEL SERIALIZABLE;
SAVEPOINT s;
SELECT s::int2 FROM ssi_src WHERE id = 1;
ROLLBACK TO SAVEPOINT s;
INSERT INTO ssi_log VALUES (2, 'after leader error');
COMMIT;
-- 3: a PL/pgSQL EXCEPTION block around an ordinary parallel plan whose workers raise the error
SET parallel_leader_participation = off; SET parallel_setup_cost = 0; SET parallel_tuple_cost = 0; SET min_parallel_table_scan_size = 0;
BEGIN ISOLATION LEVEL SERIALIZABLE;
DO $$
BEGIN
  BEGIN
    PERFORM sum(s::int2) FROM ssi_src;
  EXCEPTION WHEN invalid_text_representation THEN
    RAISE NOTICE 'caught %', SQLERRM;
  END;
  INSERT INTO ssi_log VALUES (3, 'after caught worker error');
END $$;
COMMIT;
RESET parallel_leader_participation; RESET parallel_setup_cost; RESET parallel_tuple_cost; RESET min_parallel_table_scan_size;
SELECT id, note FROM ssi_log ORDER BY id;

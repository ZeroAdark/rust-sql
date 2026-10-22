# TRIGGERS (unit A): AFTER row triggers on pgrcolumnar with a concurrent writer, against the heap twin (no expected
# file: the twin's output is the expected output). The AFTER ROW UPDATE / DELETE trigger's OLD and NEW are refetched
# from the queue after the waiter's EPQ followed the winner's version; the deferred constraint trigger fires at
# COMMIT. Targets: id 1 (sealed part) and id 11 (live segment). BEFORE ROW UPDATE / DELETE stay fenced
# (pgrc-trigger-epq pins that).

setup { CREATE TABLE pgrc_t (id int, v int, note text) USING pgrcolumnar; }
setup { INSERT INTO pgrc_t SELECT g, 0, 'orig' FROM generate_series(1, 10) g; }
setup { CALL pgrc_seal('pgrc_t'); }
setup { INSERT INTO pgrc_t SELECT g, 0, 'orig' FROM generate_series(11, 20) g; }
setup
{
  CREATE FUNCTION pgrc_atrig() RETURNS trigger LANGUAGE plpgsql AS $$
  BEGIN
    RAISE NOTICE '% % %: old % new %', TG_NAME, TG_WHEN, TG_OP, OLD, NEW;
    RETURN NULL;
  END $$;
  CREATE TRIGGER pgrc_a AFTER UPDATE OR DELETE ON pgrc_t FOR EACH ROW EXECUTE FUNCTION pgrc_atrig();
  CREATE CONSTRAINT TRIGGER pgrc_d AFTER UPDATE OR DELETE ON pgrc_t DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION pgrc_atrig();
}

teardown
{
  DROP TABLE pgrc_t;
  DROP FUNCTION pgrc_atrig();
}

session s1
step s1b    { BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1p    { UPDATE pgrc_t SET v = v + 1 WHERE id = 1 RETURNING id, v, note; }
step s1s    { UPDATE pgrc_t SET v = v + 1 WHERE id = 11 RETURNING id, v, note; }
step s1d    { DELETE FROM pgrc_t WHERE id = 11 RETURNING id, v; }
step s1c    { COMMIT; }
step s1a    { ROLLBACK; }

session s2
step s2rc   { BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2rr   { BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM pgrc_t; }
step s2p    { UPDATE pgrc_t SET v = v + 10 WHERE id = 1 RETURNING id, v, note; }
step s2s    { UPDATE pgrc_t SET v = v + 10 WHERE id = 11 RETURNING id, v, note; }
step s2ds   { DELETE FROM pgrc_t WHERE id = 11 RETURNING id, v; }
step s2c    { COMMIT; }

session r
step rseal  { CALL pgrc_seal('pgrc_t'); }
step rsel   { SELECT id, v, note FROM pgrc_t WHERE id IN (1, 11) ORDER BY id; }

permutation s1b s1p s2rc s2p s1c s2c rsel
permutation s1b s1s s2rc s2s s1c s2c rsel
permutation s1b s1s s2rc s2s s1a s2c rsel
permutation s1b s1d s2rc s2s s1c s2c rsel
permutation s1b s1s s2rc s2ds s1c s2c rsel
permutation s1b s1s rseal s1c rsel
permutation s2rr s1b s1p s2p s1c s2c rsel

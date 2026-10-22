# pgrc F2 L5 (docs/design/pgrc-delta-v2/F2.md): row triggers with a
# concurrent updater. A BEFORE ROW UPDATE trigger's OLD comes from
# get_tuple_for_trigger's lock of the newest version (the waiter's EPQ); the
# AFTER ROW trigger's OLD and NEW are refetched from the queue (the refetch seam
# of F2 L6). Both report through NOTICEs. Targets: id 1 (sealed part) and id 11
# (live segment).
#
# F2 close (2026-09-26): F2 L6 was shelved and row triggers on pgrcolumnar stayed refused. TRIGGERS (2026-10-03)
# served AFTER row triggers; LOCK (2026-10-03) serves BEFORE ROW UPDATE / DELETE: get_tuple_for_trigger locks the row
# (a LOCK row) before the trigger runs. Expected output: the heap twin again (scripts/pgrc-isolation.sh); until LOCK
# it was every UPDATE refused (0A000), pinned by the expected file LOCK deleted.

setup { CREATE TABLE pgrc_t (id int, v int, note text) USING pgrcolumnar; }
setup { INSERT INTO pgrc_t SELECT g, 0, 'orig' FROM generate_series(1, 10) g; }
setup { CALL pgrc_seal('pgrc_t'); }
setup { INSERT INTO pgrc_t SELECT g, 0, 'orig' FROM generate_series(11, 20) g; }
setup
{
  CREATE FUNCTION pgrc_trig() RETURNS trigger LANGUAGE plpgsql AS $$
  BEGIN
    RAISE NOTICE '% %: old % new %', TG_WHEN, TG_OP, OLD, NEW;
    IF TG_WHEN = 'BEFORE' THEN
      NEW.note = OLD.note || '+' || NEW.v;
      RETURN NEW;
    END IF;
    RETURN NULL;
  END $$;
  CREATE TRIGGER pgrc_b BEFORE UPDATE ON pgrc_t FOR EACH ROW EXECUTE FUNCTION pgrc_trig();
  CREATE TRIGGER pgrc_a AFTER UPDATE ON pgrc_t FOR EACH ROW EXECUTE FUNCTION pgrc_trig();
}

teardown
{
  DROP TABLE pgrc_t;
  DROP FUNCTION pgrc_trig();
}

session s1
step s1b    { BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1p    { UPDATE pgrc_t SET v = v + 1 WHERE id = 1 RETURNING id, v, note; }
step s1s    { UPDATE pgrc_t SET v = v + 1 WHERE id = 11 RETURNING id, v, note; }
step s1c    { COMMIT; }
step s1a    { ROLLBACK; }

session s2
step s2rc   { BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2rr   { BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM pgrc_t; }
step s2p    { UPDATE pgrc_t SET v = v + 10 WHERE id = 1 RETURNING id, v, note; }
step s2s    { UPDATE pgrc_t SET v = v + 10 WHERE id = 11 RETURNING id, v, note; }
step s2c    { COMMIT; }

session r
step rsel   { SELECT id, v, note FROM pgrc_t WHERE id IN (1, 11) ORDER BY id; }

permutation s1b s1p s2rc s2p s1c s2c rsel
permutation s1b s1s s2rc s2s s1c s2c rsel
permutation s1b s1s s2rc s2s s1a s2c rsel
permutation s2rr s1b s1p s2p s1c s2c rsel

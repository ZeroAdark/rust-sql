# LOCK (pgrc delta-store, 2026-10-03): BEFORE ROW UPDATE / DELETE triggers on a pgrcolumnar table against concurrent
# writers, as on heap. The trigger runs on the row get_tuple_for_trigger locked (a LOCK row): a second writer waits for
# the first, then (READ COMMITTED) runs its trigger on the newest version, or skips a deleted row, or (REPEATABLE READ)
# fails 40001. A BEFORE trigger that returns NULL skips the write but keeps its lock to the transaction's end (the
# LOCK row stays until compile drops it), so a writer behind it still waits. Rows: id 1 in a sealed part, id 11 in the
# live segment. Expected output: the heap twin (scripts/pgrc-isolation.sh).

setup { CREATE TABLE pgrc_t (id int, v int, note text) USING pgrcolumnar; }
setup { INSERT INTO pgrc_t SELECT g, 0, 'orig' FROM generate_series(1, 10) g; }
setup { CALL pgrc_seal('pgrc_t'); }
setup { INSERT INTO pgrc_t SELECT g, 0, 'orig' FROM generate_series(11, 20) g; }
setup
{
  CREATE FUNCTION pgrc_btrig() RETURNS trigger LANGUAGE plpgsql AS $$
  BEGIN
    RAISE NOTICE '% %: old %', TG_WHEN, TG_OP, OLD;
    IF TG_OP = 'DELETE' THEN
      IF OLD.note = 'keep' THEN RETURN NULL; END IF;
      RETURN OLD;
    END IF;
    IF NEW.note = 'skip' THEN RETURN NULL; END IF;
    NEW.note = OLD.note || '+' || NEW.v;
    RETURN NEW;
  END $$;
  CREATE TRIGGER pgrc_bu BEFORE UPDATE OR DELETE ON pgrc_t FOR EACH ROW EXECUTE FUNCTION pgrc_btrig();
}

teardown
{
  DROP TABLE pgrc_t;
  DROP FUNCTION pgrc_btrig();
}

session s1
step s1b    { BEGIN; }
step s1up   { UPDATE pgrc_t SET v = v + 1 WHERE id = 1 RETURNING id, v, note; }
step s1us   { UPDATE pgrc_t SET v = v + 1 WHERE id = 11 RETURNING id, v, note; }
step s1dp   { DELETE FROM pgrc_t WHERE id = 1 RETURNING id; }
step s1skip { UPDATE pgrc_t SET note = 'skip' WHERE id = 11 RETURNING id; }
step s1keep { UPDATE pgrc_t SET note = 'keep' WHERE id = 1; }
step s1c    { COMMIT; }
step s1a    { ROLLBACK; }

session s2
step s2rc   { BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2rr   { BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM pgrc_t; }
step s2up   { UPDATE pgrc_t SET v = v + 10 WHERE id = 1 RETURNING id, v, note; }
step s2us   { UPDATE pgrc_t SET v = v + 10 WHERE id = 11 RETURNING id, v, note; }
step s2dp   { DELETE FROM pgrc_t WHERE id = 1 RETURNING id; }
step s2ds   { DELETE FROM pgrc_t WHERE id = 11 RETURNING id; }
step s2c    { COMMIT; }

session r
step rsel   { SELECT id, v, note FROM pgrc_t WHERE id IN (1, 11) ORDER BY id; }

permutation s1b s1up s2rc s2dp s1c s2c rsel
permutation s1b s1dp s2rc s2up s1c s2c rsel
permutation s1b s1us s2rc s2ds s1a s2c rsel
permutation s1b s1us s2rr s2us s1c s2c rsel
# a BEFORE trigger skips the update: its lock stays, the writer behind waits for COMMIT
permutation s1b s1skip s2rc s2us s1c s2c rsel
# a BEFORE DELETE trigger skips the delete of a row its transaction updated
permutation s1keep s1b s1dp s2rc s2up s1c s2c rsel

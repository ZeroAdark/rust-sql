# R15-RR-CLAIM (ROAD2 finding, main 10-15; same root as the P0 PGRC-SORTED-CLAIM-SNAPSHOT): a REPEATABLE READ reader opened before
# pgrc_compact 'sorted' must not claim key order from the new generation. The table declares sorted_by = 'id' and
# holds two front-sealed parts (INSERTs through an AFTER ROW trigger, so the rows go to the front segment, then
# pgrc_seal: each part keeps heap order and attests no key). The reader
# takes its snapshot, another session compacts the table to one attested part, then the reader plans ORDER BY id:
# its own generation's parts attest nothing, so its plan sorts (pgrc_rrc_sorts = the plan holds a Sort node, as the
# heap twin's always does) and its rows come in key order. After it ends, a new reader claims the order (one
# attested part: no Sort) with the same rows. The AM name is split so the twin's pgrcolumnar -> heap rewrite leaves
# the checks alone.
# Expected output: the heap twin (scripts/pgrc-isolation.sh), except pgrc_rrc_sorts answers true on the twin there
# (heap never claims), so the last step asks it only on the pgrc table (pgrc_rrc_claims: false on the twin).

setup { CREATE TABLE pgrc_rrc (id int, v text) USING pgrcolumnar; }
setup
{
  CREATE FUNCTION pgrc_rrc_is_pgrc(t regclass) RETURNS boolean LANGUAGE sql STABLE AS
    $$ SELECT a.amname = 'pgrc' || 'olumnar' FROM pg_class c JOIN pg_am a ON a.oid = c.relam WHERE c.oid = t $$;
  CREATE FUNCTION pgrc_rrc_compact(regclass, text) RETURNS int LANGUAGE internal STRICT VOLATILE AS 'pgrc_compact';
  CREATE PROCEDURE pgrc_rrc_sort(t regclass) LANGUAGE plpgsql AS $$
  BEGIN
    IF pgrc_rrc_is_pgrc(t) THEN
      PERFORM pgrc_rrc_compact(t, 'sorted');
    END IF;
  END $$;
  CREATE FUNCTION pgrc_rrc_sorts(q text) RETURNS boolean LANGUAGE plpgsql AS $$
  DECLARE l text;
  BEGIN
    FOR l IN EXECUTE 'EXPLAIN (COSTS OFF) ' || q LOOP
      IF l ~ '^\s*(->\s+)?Sort\s*$' THEN RETURN true; END IF;
    END LOOP;
    RETURN false;
  END $$;
  CREATE FUNCTION pgrc_rrc_claims(t regclass, q text) RETURNS boolean LANGUAGE sql AS
    $$ SELECT pgrc_rrc_is_pgrc(t) AND NOT pgrc_rrc_sorts(q) $$;
  CREATE FUNCTION pgrc_rrc_noop() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$;
}
setup { DO $$ BEGIN IF pgrc_rrc_is_pgrc('pgrc_rrc') THEN EXECUTE 'ALTER TABLE pgrc_rrc SET (sorted_by = ''id'')'; END IF; END $$; }
setup { CREATE TRIGGER pgrc_rrc_front AFTER INSERT ON pgrc_rrc FOR EACH ROW EXECUTE FUNCTION pgrc_rrc_noop(); }
setup { INSERT INTO pgrc_rrc SELECT (g * 7) % 20 * 2 + 1, 'a' || g FROM generate_series(0, 19) g; }
setup { CALL pgrc_seal('pgrc_rrc'); }
setup { INSERT INTO pgrc_rrc SELECT (g * 7) % 20 * 2 + 2, 'b' || g FROM generate_series(0, 19) g; }
setup { CALL pgrc_seal('pgrc_rrc'); }
setup { DROP TRIGGER pgrc_rrc_front ON pgrc_rrc; }

teardown
{
  DROP TABLE pgrc_rrc;
  DROP FUNCTION pgrc_rrc_noop();
  DROP FUNCTION pgrc_rrc_claims(regclass, text);
  DROP FUNCTION pgrc_rrc_sorts(text);
  DROP PROCEDURE pgrc_rrc_sort(regclass);
  DROP FUNCTION pgrc_rrc_compact(regclass, text);
  DROP FUNCTION pgrc_rrc_is_pgrc(regclass);
}

session r
step rrr    { BEGIN ISOLATION LEVEL REPEATABLE READ; }
step rcount { SELECT count(*), min(id), max(id) FROM pgrc_rrc; }
step rsorts { SELECT pgrc_rrc_sorts('SELECT id, v FROM pgrc_rrc ORDER BY id LIMIT 10'); }
step rlim   { SELECT id, v FROM pgrc_rrc ORDER BY id LIMIT 10; }
step rall   { SELECT id FROM pgrc_rrc ORDER BY id; }
step rc     { COMMIT; }

session m
step mcomp  { CALL pgrc_rrc_sort('pgrc_rrc'); }

session n
step nclaim { SELECT pgrc_rrc_claims('pgrc_rrc', 'SELECT id, v FROM pgrc_rrc ORDER BY id LIMIT 10') = pgrc_rrc_is_pgrc('pgrc_rrc') AS as_expected; }
step nlim   { SELECT id, v FROM pgrc_rrc ORDER BY id LIMIT 10; }

# the reader's snapshot predates the compaction: no claim from the new generation
permutation rrr rcount mcomp rsorts rlim rall rc nclaim nlim
# no reader across it: the compacted table claims
permutation mcomp nclaim nlim rrr rlim rall rc

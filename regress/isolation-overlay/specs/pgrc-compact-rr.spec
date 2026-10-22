# pgrc K1 L4 (docs/design/pgrc-delta-v2 K1.md): a REPEATABLE READ reader across a compaction's publish and its
# reclaim sees the same rows. The table has four sealed parts (ids 1-40, ids 5 and 25 deleted and compiled).
# pgrc_k1_squash(t, kind) runs one forced compaction job (pgrc_compact) on a pgrcolumnar table and nothing on the
# heap twin; pgrc_seal (pgrc_flush: rotate, seal, compile, reclaim) after it is the reclaim, which must keep the
# inputs the reader's generation still reads (the reader holds the horizon). Writes after the reader's snapshot stay
# tombstones under it and fold into the output's DV once it ends. The AM name is split so the twin's
# pgrcolumnar -> heap rewrite leaves the check alone.
# Expected output: the heap twin (scripts/pgrc-isolation.sh).

setup { CREATE TABLE pgrc_t (id int, v int) USING pgrcolumnar; }
setup { INSERT INTO pgrc_t SELECT g, 0 FROM generate_series(1, 10) g; }
setup { CALL pgrc_seal('pgrc_t'); }
setup { INSERT INTO pgrc_t SELECT g, 0 FROM generate_series(11, 20) g; }
setup { CALL pgrc_seal('pgrc_t'); }
setup { INSERT INTO pgrc_t SELECT g, 0 FROM generate_series(21, 30) g; }
setup { CALL pgrc_seal('pgrc_t'); }
setup { INSERT INTO pgrc_t SELECT g, 0 FROM generate_series(31, 40) g; }
setup { CALL pgrc_seal('pgrc_t'); }
setup { DELETE FROM pgrc_t WHERE id IN (5, 25); }
setup { CALL pgrc_seal('pgrc_t'); }
setup
{
  CREATE FUNCTION pgrc_k1_compact(regclass, text) RETURNS int LANGUAGE internal STRICT VOLATILE AS 'pgrc_compact';
  CREATE PROCEDURE pgrc_k1_squash(t regclass, kind text) LANGUAGE plpgsql AS $$
  BEGIN
    IF (SELECT a.amname FROM pg_class c JOIN pg_am a ON a.oid = c.relam WHERE c.oid = t) = 'pgrc' || 'olumnar' THEN
      PERFORM pgrc_k1_compact(t, kind);
    END IF;
  END $$;
}

teardown
{
  DROP TABLE pgrc_t;
  DROP PROCEDURE pgrc_k1_squash(regclass, text);
  DROP FUNCTION pgrc_k1_compact(regclass, text);
}

session s1
step s1rr   { BEGIN ISOLATION LEVEL REPEATABLE READ; }
step s1agg  { SELECT count(*), sum(v), sum(id), min(id), max(id) FROM pgrc_t; }
step s1some { SELECT id, v FROM pgrc_t WHERE id % 10 IN (1, 2, 5) ORDER BY id; }
step s1c    { COMMIT; }

session w
step wu     { UPDATE pgrc_t SET v = v + 1 WHERE id IN (1, 21); }
step wd     { DELETE FROM pgrc_t WHERE id IN (2, 32); }

session m
step mcat   { CALL pgrc_k1_squash('pgrc_t', 'concat'); }
step mdv    { CALL pgrc_k1_squash('pgrc_t', 'dv'); }
step mflush { CALL pgrc_seal('pgrc_t'); }

session r
step ragg   { SELECT count(*), sum(v), sum(id), min(id), max(id) FROM pgrc_t; }
step rsome  { SELECT id, v FROM pgrc_t WHERE id % 10 IN (1, 2, 5) ORDER BY id; }

# the job and its reclaim under the reader
permutation s1rr s1agg mcat mflush s1agg s1some s1c ragg rsome
# writes after the reader's snapshot, then the job; they fold into the output after the reader ends
permutation s1rr s1some wu wd mflush mcat mflush s1agg s1some s1c mflush mdv mflush ragg rsome
# the reader starts between the job and its reclaim
permutation mcat s1rr s1agg mflush s1some wu wd mflush mdv s1agg s1c ragg rsome

# pgrc K1 L4 (docs/design/pgrc-delta-v2 K1.md): a READ COMMITTED updater whose target moves into a compacted part
# in the middle of its statement. s1's UPDATE scans id 11 and is held in its SET list by pgrc_gate (advisory lock 1,
# held by session c); meanwhile w updates the same row and commits, and m seals w's new version and compacts every
# part (the old version's and the successor's) into one (pgrc_k1_squash: one forced pgrc_compact job on a
# pgrcolumnar table, nothing on the heap twin). When the gate opens, s1 finds its target updated, follows it to the
# successor through the output's row map (EPQ), and updates that one: one new version, the count unchanged.
# Expected output: the heap twin (scripts/pgrc-isolation.sh).

setup { CREATE TABLE pgrc_t (id int, v int) USING pgrcolumnar; }
setup { INSERT INTO pgrc_t SELECT g, 0 FROM generate_series(1, 10) g; }
setup { CALL pgrc_seal('pgrc_t'); }
setup { INSERT INTO pgrc_t SELECT g, 0 FROM generate_series(11, 20) g; }
setup { CALL pgrc_seal('pgrc_t'); }
setup
{
  CREATE FUNCTION pgrc_gate(k int, v int) RETURNS int LANGUAGE sql VOLATILE
    AS 'SELECT pg_advisory_xact_lock_shared(k); SELECT v';
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
  DROP FUNCTION pgrc_gate(int, int);
  DROP PROCEDURE pgrc_k1_squash(regclass, text);
  DROP FUNCTION pgrc_k1_compact(regclass, text);
}

session c
step chold  { SELECT pg_advisory_lock(1); }
step cgo    { SELECT pg_advisory_unlock(1); }

session s1
step s1rc   { BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1ug   { UPDATE pgrc_t SET v = pgrc_gate(1, v + 1) WHERE id = 11 RETURNING id, v; }
step s1dg   { DELETE FROM pgrc_t WHERE id = pgrc_gate(1, 11) RETURNING id, v; }
step s1c    { COMMIT; }

session w
step wu     { UPDATE pgrc_t SET v = v + 10 WHERE id = 11; }

session m
step mseal  { CALL pgrc_seal('pgrc_t'); }
step mcat   { CALL pgrc_k1_squash('pgrc_t', 'concat'); }

session r
step rsel   { SELECT id, v FROM pgrc_t WHERE id IN (1, 11) ORDER BY id; }
step rcount { SELECT count(*), sum(v), count(DISTINCT id) FROM pgrc_t; }

# the target's part is compacted under the held UPDATE, no concurrent writer
permutation chold s1rc s1ug mcat cgo s1c rsel rcount
# a writer's successor, sealed and compacted with the target's part, found by EPQ
permutation chold s1rc s1ug wu mseal mcat cgo s1c rsel rcount
# the same for a DELETE
permutation chold s1rc s1dg wu mseal mcat cgo s1c rsel rcount

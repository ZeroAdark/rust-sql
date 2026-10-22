# PGRCSERVE M2: index, index-only and bitmap scans of a pgrcolumnar table under a concurrent seal (the front rows move
# into a part) and a compaction (the two parts merge). A row's ID is stable through both, so the index entries stay
# right: a REPEATABLE READ reader that started before sees the same rows across both, and readers after see them too.
# Rows 1-200 sit in two parts, rows 201-260 in the front segment (COPY through an AFTER ROW trigger), so no INSERT is
# needed. gs_squash runs one forced compaction on the pgrcolumnar table and nothing on the heap twin (the AM name is
# split so the twin's rewrite leaves the check alone). Expected output: the heap twin (scripts/pgrc-isolation.sh).

setup { CREATE OR REPLACE FUNCTION gs_noop() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$; }
setup { CREATE TABLE gs_ix (id int, v int NOT NULL DEFAULT 7) USING pgrcolumnar; }
setup { COPY gs_ix (id) FROM PROGRAM 'seq 1 100'; }
setup { CALL pgrc_seal('gs_ix'); }
setup { COPY gs_ix (id) FROM PROGRAM 'seq 101 200'; }
setup { CALL pgrc_seal('gs_ix'); }
setup { CREATE TRIGGER gs_front AFTER INSERT ON gs_ix FOR EACH ROW EXECUTE FUNCTION gs_noop(); }
setup { COPY gs_ix (id) FROM PROGRAM 'seq 201 260'; }
setup { DROP TRIGGER gs_front ON gs_ix; }
setup { CREATE INDEX gs_ix_id ON gs_ix (id); ANALYZE gs_ix; }
setup
{
  CREATE FUNCTION gs_compact(regclass, text) RETURNS int LANGUAGE internal STRICT VOLATILE AS 'pgrc_compact';
  CREATE FUNCTION gs_segrows(regclass) RETURNS bigint LANGUAGE internal STRICT VOLATILE AS 'pgrc_segment_rows';
  CREATE FUNCTION gs_front(t regclass) RETURNS bigint LANGUAGE plpgsql AS $$
  BEGIN
    IF (SELECT a.amname FROM pg_class c JOIN pg_am a ON a.oid = c.relam WHERE c.oid = t) = 'pgrc' || 'olumnar' THEN
      RETURN gs_segrows(t);
    END IF;
    RETURN 0;
  END $$;
  CREATE PROCEDURE gs_squash(t regclass, kind text) LANGUAGE plpgsql AS $$
  BEGIN
    IF (SELECT a.amname FROM pg_class c JOIN pg_am a ON a.oid = c.relam WHERE c.oid = t) = 'pgrc' || 'olumnar' THEN
      PERFORM gs_compact(t, kind);
    END IF;
  END $$;
}

teardown
{
  DROP TABLE gs_ix;
  DROP PROCEDURE gs_squash(regclass, text);
  DROP FUNCTION gs_compact(regclass, text);
  DROP FUNCTION gs_front(regclass);
  DROP FUNCTION gs_segrows(regclass);
  DROP FUNCTION gs_noop();
}

session s1
setup       { SET enable_seqscan = off; SET enable_bitmapscan = off; }
step s1rr   { BEGIN ISOLATION LEVEL REPEATABLE READ; }
step s1pt   { SELECT id, v FROM gs_ix WHERE id IN (50, 150, 230, 261) ORDER BY id; }
step s1rg   { SELECT count(*), sum(id), min(id), max(id) FROM gs_ix WHERE id BETWEEN 90 AND 240; }
step s1c    { COMMIT; }
step s1ex   { EXPLAIN (COSTS OFF) SELECT id, v FROM gs_ix WHERE id IN (50, 150, 230, 261) ORDER BY id; EXPLAIN (COSTS OFF) SELECT count(*), sum(id), min(id), max(id) FROM gs_ix WHERE id BETWEEN 90 AND 240; }

session b
setup       { SET enable_seqscan = off; SET enable_indexscan = off; SET enable_indexonlyscan = off; }
step bsel   { SELECT count(*), sum(v), min(id), max(id) FROM gs_ix WHERE id BETWEEN 95 AND 205 OR id > 255; }
step bex    { EXPLAIN (COSTS OFF) SELECT count(*), sum(v), min(id), max(id) FROM gs_ix WHERE id BETWEEN 95 AND 205 OR id > 255; }

session m
step mseal  { CALL pgrc_seal('gs_ix'); SELECT gs_front('gs_ix') AS front_rows; }
step mcat   { CALL gs_squash('gs_ix', 'concat'); }

session r
setup       { SET enable_seqscan = off; SET enable_bitmapscan = off; }
step rpt    { SELECT id, v FROM gs_ix WHERE id IN (50, 150, 230, 261) ORDER BY id; }
step rrg    { SELECT count(*), sum(id), min(id), max(id) FROM gs_ix WHERE id BETWEEN 90 AND 240; }

# a reader across the seal and the compaction
permutation s1ex bex s1rr s1pt s1rg bsel mseal s1pt s1rg bsel mcat s1pt s1rg bsel s1c rpt rrg
# readers after each
permutation rpt rrg bsel mseal rpt rrg bsel mcat rpt rrg bsel

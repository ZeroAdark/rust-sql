# COMPACTP item 2 (b) (pgrc-plans/COMPACTP.done): a compaction publishes over another transaction's open bulk load
# instead of skipping or deferring to it (K1 choice 8 before): the load only added parts, so the compaction's generation
# carries them tagged with their creator (EXT_UNDER) and retires committed parts below them. Every reader resolves the
# tags by its own snapshot. The load then commits or rolls back (the next publish drops its tag, or its parts: GENID
# keeps the order); a REPEATABLE READ snapshot taken before the compaction still reads the retired inputs; the open
# loader loads again over the compaction's generation. pgrc_cs_compact runs one forced concat job (pgrc_compact) on the
# pgrcolumnar table and answers whether it ran one (true on the heap twin, where it does nothing): the line skipped the
# job while the load was open (false). Every loader goes through the bulk lane (pgrust.pgrc_bulk_rows = 5: a 10-row
# INSERT publishes a part). The heap twin is the oracle. A read's count and sum name the loads it sees: the setup's
# 60 rows sum to 1830, a's to 10055, a2's to 10155, b's to 20055 (an ungrouped read: the Gravity/SQE slots serve it).

setup
{
    CREATE TABLE pgrc_cs (id int8, v text) USING pgrcolumnar;
    CREATE FUNCTION pgrc_cs_compact(t regclass) RETURNS bool LANGUAGE plpgsql AS $$
    BEGIN
        -- not "= the AM's name": the runner's heap twin rewrites that word, which would turn the test into "is it heap"
        IF (SELECT a.amname FROM pg_class c JOIN pg_am a ON a.oid = c.relam WHERE c.oid = t) <> 'heap' THEN
            RETURN pgrc_compact(t, 'concat') > 0;
        END IF;
        RETURN true;
    END $$;
}
setup
{
    SET pgrust.pgrc_bulk_rows = 5;
    INSERT INTO pgrc_cs SELECT g, 's1' FROM generate_series(1, 10) g;
    INSERT INTO pgrc_cs SELECT g, 's2' FROM generate_series(11, 20) g;
    INSERT INTO pgrc_cs SELECT g, 's3' FROM generate_series(21, 30) g;
    INSERT INTO pgrc_cs SELECT g, 's4' FROM generate_series(31, 40) g;
    INSERT INTO pgrc_cs SELECT g, 's5' FROM generate_series(41, 50) g;
    INSERT INTO pgrc_cs SELECT g, 's6' FROM generate_series(51, 60) g;
}
teardown { DROP TABLE pgrc_cs; DROP FUNCTION pgrc_cs_compact(regclass); }

session a
setup    { SET pgrust.pgrc_bulk_rows = 5; }
step ab  { BEGIN; }
step ai  { INSERT INTO pgrc_cs SELECT g, 'a' FROM generate_series(1001, 1010) g; }
step ai2 { INSERT INTO pgrc_cs SELECT g, 'a2' FROM generate_series(1011, 1020) g; }
step ar  { SELECT count(*), sum(id) FROM pgrc_cs; }
step ac  { COMMIT; }
step aa  { ROLLBACK; }

session b
setup    { SET pgrust.pgrc_bulk_rows = 5; }
step bi  { INSERT INTO pgrc_cs SELECT g, 'b' FROM generate_series(2001, 2010) g; }

session m
step mc  { SELECT pgrc_cs_compact('pgrc_cs') AS compacted; }

session r
step rb  { BEGIN ISOLATION LEVEL REPEATABLE READ; }
step rs  { SELECT count(*), sum(id) FROM pgrc_cs; }
step rc  { COMMIT; }

# the compaction stacks over a's open load; a rolls back; the next load drops a's part
permutation ab ai mc rs aa rs bi rs
# the same, a commits: its rows stay, under the compaction's generation
permutation ab ai mc rs ac rs bi rs
# a REPEATABLE READ snapshot from before the load and the compaction still reads the retired inputs
permutation rb rs ab ai mc rs ac rs rc rs
# a loads again over the compaction's generation (its own part there untagged), reads its own rows, then each outcome
permutation ab ai mc ai2 ar ac rs bi rs
permutation ab ai mc ai2 ar aa rs bi rs
# two compactions over one open load
permutation ab ai mc bi mc rs aa rs

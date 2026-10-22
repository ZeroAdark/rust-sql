# D4854: one lowering read a pgrc relation's jsonb path elections twice. jsonb.lanes' site draft (jsonb_lanes::engage)
# declares one Text shred per path jsonb_lanes::paths lists, and a path group key's representative lane is its place in a
# second read of the same list (jsonb_keys::key_path). A compaction published between the two reads moved the elections:
# the second list lost path 'a' (the first part to elect it then elected a number), so key 'b' took shred 0, path 'a''s
# lane, and every group a part 4 row reached rendered 'a''s text as its 'b' (WRONG rows). The fix pins each pgrc
# relation's resident part list at a lowering's first read (hosts facts.rs RelcacheFacts::resident).
#   the hook: gravity-shred-election-read (RelcacheFacts::shred_election, after each pgrc election read) holds s1's
#     lowering after its first read; s2 commits one concat job and wakes s1. A PGRUST_TEST_MODULES=1 build.
#   the layout: parts 1 and 2 small, 'a' a string (part 2 has one number, so it elects no 'a'); parts 3 ('a' a number) and
#     4 ('a' a string) full (1 Mi rows each), so concat's window is parts 1 + 2, and the worker, whose fan-in is 4, leaves
#     the two alone. The merged part elects no 'a': part 3's number is then the first election.
# The heap twin never waits at the pgrc hook (its wakeup finds no waiter): the expected output is the overlay's (the twin's
# rows, with the hook's wait).

setup { CREATE EXTENSION injection_points; }
setup { CREATE TABLE pgrc_d4854 (i int8, j jsonb) USING pgrcolumnar; }
setup { SET pgrust.pgrc_bulk_rows = 1; INSERT INTO pgrc_d4854 SELECT g, jsonb_build_object('a', 'x' || (g % 3), 'b', 'y' || (g % 2)) FROM generate_series(1, 1000) g; }
setup { SET pgrust.pgrc_bulk_rows = 1; INSERT INTO pgrc_d4854 SELECT g, jsonb_build_object('a', CASE WHEN g = 1500 THEN to_jsonb(7) ELSE to_jsonb('x' || (g % 3)) END, 'b', 'y' || (g % 2)) FROM generate_series(1001, 2000) g; }
setup { SET pgrust.pgrc_bulk_rows = 1; INSERT INTO pgrc_d4854 SELECT g, jsonb_build_object('a', g % 7, 'b', 'y' || (g % 2)) FROM generate_series(2001, 1050576) g; }
setup { SET pgrust.pgrc_bulk_rows = 1; INSERT INTO pgrc_d4854 SELECT g, jsonb_build_object('a', 'x' || (g % 3), 'b', 'y' || (g % 2)) FROM generate_series(1050577, 2099152) g; }

teardown
{
    DROP TABLE pgrc_d4854;
    DO $$ BEGIN PERFORM injection_points_detach('gravity-shred-election-read'); EXCEPTION WHEN OTHERS THEN NULL; END $$;
    DROP EXTENSION injection_points;
}

session s1
step s1q { SELECT j ->> 'a' AS a, j ->> 'b' AS b, count(*) FROM pgrc_d4854 GROUP BY 1, 2 ORDER BY 1, 2; }

session s2
step s2a { SELECT injection_points_attach('gravity-shred-election-read', 'wait'); }
step s2c
{
    DO $$ BEGIN
        IF (SELECT a.amname FROM pg_class c JOIN pg_am a ON a.oid = c.relam WHERE c.oid = 'pgrc_d4854'::regclass) <> 'heap' THEN
            PERFORM pgrc_compact('pgrc_d4854', 'concat');
        END IF;
    END $$;
}
step s2w { SELECT injection_points_detach('gravity-shred-election-read'); SELECT injection_points_wakeup('gravity-shred-election-read'); }
step s2q { SELECT j ->> 'a' AS a, j ->> 'b' AS b, count(*) FROM pgrc_d4854 GROUP BY 1, 2 ORDER BY 1, 2; }

permutation s2a s1q s2c s2w s2q

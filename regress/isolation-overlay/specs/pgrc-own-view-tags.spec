# CONCLOAD unit 1 (pgrc-plans/CONCLOAD.done): a REPEATABLE READ loader's own view hides the parts its base generation
# tags for a creator its snapshot does not see. x's seal stacks over c's open bulk load (SEAL-OFFHEAD: c's part is
# tagged with c in x's generation) and commits; a takes its snapshot (it sees x, not c); c commits; a's bulk load
# publishes over x's generation and drops the tag of c, committed by then, so a's generation lists c's part untagged.
# a's reads must still leave c's rows out, as the heap twin's do. s holds the table's maintenance lock from before c's
# commit (pgrc_hold), so the front worker cannot settle x's generation in between (a settle would put a generation a's
# snapshot does not see under a's publish: the composed-view path, which was right already). Every loader goes
# through the bulk lane (pgrust.pgrc_bulk_rows = 100); the 50 setup rows are front rows for x to seal.

setup
{
    CREATE TABLE pgrc_ovt (id int8, v text) USING pgrcolumnar;
    INSERT INTO pgrc_ovt SELECT g, 'front' FROM generate_series(1, 50) g;
}
teardown { DROP TABLE pgrc_ovt; }

session c
setup    { SET pgrust.pgrc_bulk_rows = 100; }
step cb  { BEGIN; }
step ci  { INSERT INTO pgrc_ovt SELECT g, 'c' FROM generate_series(1001, 1500) g; }
step cc  { COMMIT; }

session x
step xs  { CALL pgrc_seal('pgrc_ovt'); }

session s
step sh  { BEGIN; CALL pgrc_hold('pgrc_ovt'); }
step sc  { COMMIT; }

session a
setup    { SET pgrust.pgrc_bulk_rows = 100; }
step ab  { BEGIN ISOLATION LEVEL REPEATABLE READ; }
step abc { BEGIN; }
step ar  { SELECT v, count(*), sum(id) FROM pgrc_ovt GROUP BY v ORDER BY v; }
step ai  { INSERT INTO pgrc_ovt SELECT g, 'a' FROM generate_series(2001, 2500) g; }
step ac  { COMMIT; }

session r
step rr  { SELECT v, count(*), sum(id) FROM pgrc_ovt GROUP BY v ORDER BY v; }

# a's reads after its publish: its own rows and the setup rows, not c's
permutation cb ci xs sh ab ar cc ai ar sc ac rr
# READ COMMITTED: a's second statement's snapshot sees c
permutation cb ci xs sh abc ar cc ai ar sc ac rr

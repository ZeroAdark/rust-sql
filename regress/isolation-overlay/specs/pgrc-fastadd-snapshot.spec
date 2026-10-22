# FASTADD (pgrc-plans/FASTADD.done): a REPEATABLE READ snapshot taken before an ADD COLUMN (or DROP COLUMN) reads the
# table after the ALTER commits. Heap's ALTER is metadata-only (attmissingval), so the reader sees every row it saw
# before, with the new column read as its default; a rewriting ALTER would leave it an empty table ("not MVCC-safe").
# pgrcolumnar's ALTER is metadata-only too: the reader's snapshot keeps its generation, whose parts lack the column,
# and the readers fill it from attmissingval. The table holds sealed parts, live front segment rows and tombstones.
#   first: ADD x DEFAULT 7, y DEFAULT 'dy' (a constant), z (NULL); the reader sees the old rows filled, not the rows
#     inserted after; after its commit it sees them
#   second: the same with a seal (flush) after the ADD: the reader still reads its own generation
#   third: DROP COLUMN k: the reader reads the remaining columns of every old row
#   fourth: ADD in a transaction that rolls back after an INSERT: the reader and later readers see no trace of it

setup
{
    CREATE TABLE pgrc_fa (id int8, k int, v text) USING pgrcolumnar;
    CREATE TABLE pgrc_fa_other (id int8);
}
setup { SET pgrust.pgrc_bulk_rows = 100; INSERT INTO pgrc_fa SELECT g, g % 7, 'p' || g FROM generate_series(1, 600) g; }
setup { INSERT INTO pgrc_fa SELECT g, g % 7, 'f' || g FROM generate_series(601, 640) g; DELETE FROM pgrc_fa WHERE id % 50 = 0; }

teardown { DROP TABLE pgrc_fa, pgrc_fa_other; }

session r
step rb    { BEGIN ISOLATION LEVEL REPEATABLE READ; }
step rsnap { SELECT count(*) FROM pgrc_fa_other; }
step rs    { SELECT count(*), count(x), sum(x), min(y), max(y), count(z), sum(id) FROM pgrc_fa; }
step rk    { SELECT count(*), sum(id), max(v) FROM pgrc_fa; }
step rc    { COMMIT; }

session a
step aadd  { ALTER TABLE pgrc_fa ADD COLUMN x int DEFAULT 7, ADD COLUMN y text DEFAULT 'dy', ADD COLUMN z int; }
step ains  { INSERT INTO pgrc_fa (id, x, y) VALUES (9001, 8, 'new'), (9002, NULL, NULL); }
step aseal { CALL pgrc_seal('pgrc_fa'); }
step adrop { ALTER TABLE pgrc_fa DROP COLUMN k; }
step abeg  { BEGIN; }
step aadd2 { ALTER TABLE pgrc_fa ADD COLUMN w int DEFAULT 3; }
step ains2 { INSERT INTO pgrc_fa (id, w) VALUES (9100, 4); }
step arb   { ROLLBACK; }
step aw    { SELECT count(*), sum(id), count(*) FILTER (WHERE id = 9100) FROM pgrc_fa; }

permutation rb rsnap aadd ains rs rc rs
permutation rb rsnap aadd ains aseal rs rc rs
permutation rb rsnap adrop rk rc rk
permutation rb rsnap abeg aadd2 ains2 arb rk rc rk aw

# LOADWAIT (pgrc-plans/LOADWAIT.done): a TRUNCATE activates the table's front segment in its own transaction, as
# CREATE TABLE does, so the first small insert after it neither waits nor is waited on: until LOADWAIT that insert
# activated the segment in its own transaction (a publish that changes the segment set, under a ShareUpdateExclusiveLock
# held to its transaction end), a bulk load that found it in progress waited for its transaction, the activation waited
# for a bulk load in progress under it, and a second first-inserter or an ANALYZE waited on its lock (CONCLOAD's
# residual). pgrc_cr is truncated in its own transaction (a new relfilenumber); pgrc_cs is created, written and
# truncated in one transaction (the in-place arm: the directory emptied). The heap twin is the oracle. What still
# waits: pgrc-publish-deadlock's held flush.

setup { CREATE TABLE pgrc_cr (id int8, v text) USING pgrcolumnar; }
setup { TRUNCATE pgrc_cr; }
setup { CREATE TABLE pgrc_cs (id int8, v text) USING pgrcolumnar; INSERT INTO pgrc_cs VALUES (0, 'gone'); TRUNCATE pgrc_cs; }
teardown { DROP TABLE pgrc_cr, pgrc_cs; }

session f
step fb  { BEGIN; }
step fi  { INSERT INTO pgrc_cr VALUES (1, 'f'); }
step fs  { INSERT INTO pgrc_cs VALUES (1, 'f'); }
step fc  { COMMIT; }
step fa  { ROLLBACK; }

session g
step gi  { INSERT INTO pgrc_cr VALUES (2, 'g'); }

session l
setup    { SET pgrust.pgrc_bulk_rows = 100; }
step lb  { BEGIN; }
step li  { INSERT INTO pgrc_cr SELECT g, 'l' FROM generate_series(1001, 1500) g; }
step ls  { INSERT INTO pgrc_cs SELECT g, 'l' FROM generate_series(1001, 1500) g; }
step lc  { COMMIT; }

session r
step rs  { SELECT v, count(*), sum(id) FROM pgrc_cr GROUP BY v ORDER BY v; }
step rt  { SELECT v, count(*), sum(id) FROM pgrc_cs GROUP BY v ORDER BY v; }
step ra  { ANALYZE pgrc_cr; }

# the first insert open, then a load
permutation fb fi li fc rs
# a load open, then the first insert
permutation lb li fi lc rs
# a second first insert while the first is open
permutation fb fi gi fc rs
# ANALYZE while the first insert is open
permutation fb fi ra fc rs
# a load committed while the first insert is open, then that insert rolled back
permutation fb fi li fa rs
# the in-place TRUNCATE: the first insert open, then a load, and the reverse
permutation fb fs ls fc rt
permutation lb ls fs lc rt

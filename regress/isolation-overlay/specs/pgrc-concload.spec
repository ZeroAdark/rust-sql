# CONCLOAD (pgrc-plans/CONCLOAD.done): bulk loads of one pgrcolumnar table never wait for each other, as heap's
# don't. A load that finds another transaction's load in progress on the chain head publishes over it (the stacking
# probe): its generation names the creator of every part a transaction still in progress added (EXT_UNDER), and each
# reader resolves them by its snapshot. What PUBLISH-WAIT.done listed as divergences is gone: a second (third) loader
# does not wait; opposite-order loads of two tables do not deadlock; a loader that then takes LOCK TABLE ... SHARE or
# TRUNCATE waits for the other load's RowExclusiveLock, as with heap, and no longer deadlocks. Commit and abort in
# every order, REPEATABLE READ and READ COMMITTED readers and loaders, a savepoint rolled back under a stacked load.
# Every loader goes through the bulk lane (pgrust.pgrc_bulk_rows = 100: a 500-row INSERT ... SELECT publishes a part).
# The heap twin is the oracle.

setup
{
    CREATE TABLE pgrc_cl (id int8, v text) USING pgrcolumnar;
    CREATE TABLE pgrc_cm (id int8, v text) USING pgrcolumnar;
}
setup { SET pgrust.pgrc_bulk_rows = 100; INSERT INTO pgrc_cl SELECT g, 'h' FROM generate_series(1, 500) g; }
teardown { DROP TABLE pgrc_cl, pgrc_cm; }

session a
setup    { SET pgrust.pgrc_bulk_rows = 100; }
step ab  { BEGIN; }
step abr { BEGIN ISOLATION LEVEL REPEATABLE READ; }
step ai  { INSERT INTO pgrc_cl SELECT g, 'a' FROM generate_series(1001, 1500) g; }
step ai2 { INSERT INTO pgrc_cl SELECT g, 'a2' FROM generate_series(1501, 2000) g; }
step am  { INSERT INTO pgrc_cm SELECT g, 'a' FROM generate_series(1001, 1500) g; }
step asp { SAVEPOINT sp; }
step arb { ROLLBACK TO SAVEPOINT sp; }
step ar  { SELECT v, count(*), sum(id) FROM pgrc_cl GROUP BY v ORDER BY v; }
step al  { LOCK TABLE pgrc_cl IN SHARE MODE; }
step at  { TRUNCATE pgrc_cl; }
step ac  { COMMIT; }
step aa  { ROLLBACK; }

session b
setup    { SET pgrust.pgrc_bulk_rows = 100; }
step bb  { BEGIN; }
step bi  { INSERT INTO pgrc_cl SELECT g, 'b' FROM generate_series(2001, 2500) g; }
step bi2 { INSERT INTO pgrc_cl SELECT g, 'b2' FROM generate_series(2501, 3000) g; }
step bm  { INSERT INTO pgrc_cm SELECT g, 'b' FROM generate_series(2001, 2500) g; }
step bc  { COMMIT; }
step ba  { ROLLBACK; }

session c
setup    { SET pgrust.pgrc_bulk_rows = 100; }
step cb  { BEGIN; }
step ci  { INSERT INTO pgrc_cl SELECT g, 'c' FROM generate_series(3001, 3500) g; }
step cc  { COMMIT; }
step ca  { ROLLBACK; }

session r
step rb  { BEGIN ISOLATION LEVEL REPEATABLE READ; }
step rs  { SELECT v, count(*), sum(id) FROM pgrc_cl GROUP BY v ORDER BY v; }
step rc  { COMMIT; }
step rm  { SELECT v, count(*), sum(id) FROM pgrc_cm GROUP BY v ORDER BY v; }

# a second and a third loader while the first is open: no wait
permutation ab ai bb bi cb ci ac bc cc rs
# a load in autocommit over two open loads
permutation ab ai bb bi ci bc ac rs
# the first loader rolls back under the second (then the second loads again over what is left)
permutation ab ai bb bi aa bi2 bc rs
# the second rolls back, the first loads again and commits
permutation ab ai bb bi ba ai2 ac rs
# the middle one of three rolls back
permutation ab ai bb bi cb ci ba cc ac rs
# the loaders publish in turn, then commit in the order they began
permutation ab ai bb bi ai2 bi2 ac bc rs
# a REPEATABLE READ snapshot taken while both are open sees neither, across both commits
permutation ab ai bb bi rb rs bc rs ac rs rc rs
# a snapshot between the two commits: the stacked one committed first
permutation ab ai bb bi bc rb rs ac rs rc rs
# READ COMMITTED reads between the commits (the later loader committed, the one under it open)
permutation ab ai bb bi bc rs ac rs
# a READ COMMITTED loader's own reads, before and after the other one's commit
permutation ab ai bb bi ar bc ar ai2 ar ac rs
# a REPEATABLE READ loader whose snapshot is older than the other one's commit: its own reads never see it, also
# after its next load published over the other one's generation
permutation abr ai bb bi bc ar ai2 ar ac rs
# a savepoint rolled back under a stacked load
permutation ab ai asp ai2 bb bi arb ar bc ac rs
# opposite-order loads of two tables: no deadlock
permutation ab ai bb bm am bi ac bc rs rm
# the first loader then takes LOCK TABLE ... IN SHARE MODE: it waits for the second one's RowExclusiveLock only
permutation ab ai bb bi al bc ac rs
# the same with TRUNCATE
permutation ab ai bb bi at bc ac rs

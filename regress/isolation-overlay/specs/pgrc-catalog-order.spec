# CONCLOAD unit 1 (pgrc-plans/CONCLOAD.done): a publish's generation enters the process catalog under the same publish
# lock as its disk publish, so the catalog's chain order is the disk's. Before, the lock was released between the two,
# and a publish that ran in between (a seal stacked over a bulk load in progress, or any publish over a freeze) entered
# the catalog first; the delayed publisher's record then dropped it as a rolled-back generation above its base (GENID
# drop_dead): readers of the catalog lost it, and its parts went to the dead list that reclaim unlinks. The test hooks
# pgrc-bulk-published (a bulk publish, after its publish lock) and pgrc-freeze-published (VACUUM's freeze, after its
# publish lock) hold the first publisher in that window.
#   first permutation: c's bulk load waits at the hook; x's seal stacks over it; c goes on and commits; r's REPEATABLE
#     READ snapshot sees both; n's bulk load publishes over x's generation. r's next read walks past n's generation to
#     x's: an ERROR ("not in the catalog") while x's generation was lost.
#   second permutation: VACUUM FREEZE waits at the hook after its freeze publish; n's bulk load publishes over the frozen
#     generation and commits; the VACUUM goes on. n's rows were lost to every reader of the catalog.
# The heap twin never waits at a pgrcolumnar hook: the expected output is the overlay's (the twin's rows, with the
# two hook waits).

setup { CREATE EXTENSION injection_points; }
setup
{
    CREATE TABLE pgrc_co (id int8, v text) USING pgrcolumnar;
    INSERT INTO pgrc_co SELECT g, 'front' FROM generate_series(1, 50) g;
    CREATE TABLE pgrc_cf (id int8, v text) USING pgrcolumnar;
}
setup { SET pgrust.pgrc_bulk_rows = 100; INSERT INTO pgrc_cf SELECT g, 'base' FROM generate_series(1, 500) g; }

teardown
{
    DROP TABLE pgrc_co, pgrc_cf;
    DO $$ BEGIN PERFORM injection_points_detach('pgrc-bulk-published'); EXCEPTION WHEN OTHERS THEN NULL; END $$;
    DO $$ BEGIN PERFORM injection_points_detach('pgrc-freeze-published'); EXCEPTION WHEN OTHERS THEN NULL; END $$;
    DROP EXTENSION injection_points;
}

session c
setup    { SET pgrust.pgrc_bulk_rows = 100; }
step cw  { SELECT injection_points_attach('pgrc-bulk-published', 'wait'); }
step cb  { BEGIN; }
step ci  { INSERT INTO pgrc_co SELECT g, 'c' FROM generate_series(1001, 1500) g; }
step cc  { COMMIT; }

session x
step xs  { CALL pgrc_seal('pgrc_co'); }

session v
step vw  { SELECT injection_points_attach('pgrc-freeze-published', 'wait'); }
step vf  { VACUUM (FREEZE) pgrc_cf; }

session w
step wb  { SELECT injection_points_detach('pgrc-bulk-published'); SELECT injection_points_wakeup('pgrc-bulk-published'); }
step wf  { SELECT injection_points_detach('pgrc-freeze-published'); SELECT injection_points_wakeup('pgrc-freeze-published'); }

session n
setup    { SET pgrust.pgrc_bulk_rows = 100; }
step ni  { INSERT INTO pgrc_co SELECT g, 'n' FROM generate_series(3001, 3500) g; }
step nf  { INSERT INTO pgrc_cf SELECT g, 'n' FROM generate_series(3001, 3500) g; }

session r
step rb  { BEGIN ISOLATION LEVEL REPEATABLE READ; }
step rs  { SELECT v, count(*), sum(id) FROM pgrc_co GROUP BY v ORDER BY v; }
step rc  { COMMIT; }
step rf  { SELECT v, count(*), sum(id) FROM pgrc_cf GROUP BY v ORDER BY v; }

permutation cw cb ci xs wb cc rb rs ni rs rc rs
permutation vw vf nf wf rf

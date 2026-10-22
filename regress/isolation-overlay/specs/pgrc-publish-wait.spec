# PUBLISH-WAIT (pgrc-plans/PUBLISH-WAIT-design.md section 5.2), CONCLOAD (pgrc-plans/CONCLOAD.done): a bulk load of a
# pgrcolumnar table that finds another transaction's load in progress on the chain head publishes over it (the
# stacking probe) instead of waiting for it, as heap's loads never wait for each other; the other load's rows count
# when it commits, none when it aborts. The holder publishes a second time over the stacked generation, and two loads
# stack over one holder. Every session loads through the bulk lane (pgrust.pgrc_bulk_rows = 100: a 500-row
# INSERT ... SELECT publishes a part). The heap twin is the oracle (until CONCLOAD the load waited: an overlay expected
# output with the waits). What still waits: pgrc-publish-deadlock's held flush (a first small insert after a
# TRUNCATE no longer does: pgrc-concload-residual, LOADWAIT).

setup { CREATE TABLE pgrc_pw (id int8, v text) USING pgrcolumnar; }
teardown { DROP TABLE pgrc_pw; }

session s1
setup     { SET pgrust.pgrc_bulk_rows = 100; }
step s1b  { BEGIN; }
step s1i  { INSERT INTO pgrc_pw SELECT g, 's1' FROM generate_series(1, 500) g; }
step s1i2 { INSERT INTO pgrc_pw SELECT g, 's1-2' FROM generate_series(501, 1000) g; }
step s1c  { COMMIT; }
step s1a  { ROLLBACK; }

session s2
setup     { SET pgrust.pgrc_bulk_rows = 100; }
step s2i  { INSERT INTO pgrc_pw SELECT g, 's2' FROM generate_series(2001, 2500) g; }

session s3
setup     { SET pgrust.pgrc_bulk_rows = 100; }
step s3i  { INSERT INTO pgrc_pw SELECT g, 's3' FROM generate_series(3001, 3500) g; }

session r
step rc   { SELECT v, count(*), sum(id) FROM pgrc_pw GROUP BY v ORDER BY v; }

# the holder commits after the second load published over its generation
permutation s1b s1i s2i s1c rc
# the holder aborts: its part leaves the stacked generation
permutation s1b s1i s2i s1a rc
# the holder publishes again, over the stacked generation, then commits
permutation s1b s1i s2i s1i2 s1c rc
# two loads stack over one holder
permutation s1b s1i s2i s3i s1c rc

# pgrc RR-VIEW (pgrc-plans/RR-VIEW-design.md): DELETE and UPDATE by a REPEATABLE READ or SERIALIZABLE transaction
# after its own publish chained on a generation committed after its snapshot. It must neither see nor change the rows
# its snapshot does not see (s2's bulk part, s2's front rows sealed by m's flush): once it commits they are all still
# there, unchanged. A concurrent UPDATE of a row it then updates is heap's 40001. Under READ COMMITTED each statement
# sees, and changes, the latest commits. The table has a part (ids 1-10) and front rows (ids 11-15).
# pgrust.pgrc_bulk_rows = 20 sends an INSERT of 20 rows or more to the bulk lane; the heap twin ignores it.
# Expected output: the heap twin (scripts/pgrc-isolation.sh).

setup { CREATE TABLE pgrc_t (id int, v int) USING pgrcolumnar; }
setup { INSERT INTO pgrc_t SELECT g, 0 FROM generate_series(1, 10) g; }
setup { CALL pgrc_seal('pgrc_t'); }
setup { INSERT INTO pgrc_t SELECT g, 0 FROM generate_series(11, 15) g; }

teardown { DROP TABLE pgrc_t; }

session s1
setup       { SET pgrust.pgrc_bulk_rows = 20; }
step s1rr   { BEGIN ISOLATION LEVEL REPEATABLE READ; }
step s1ser  { BEGIN ISOLATION LEVEL SERIALIZABLE; }
step s1rc   { BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1agg  { SELECT count(*), sum(id), sum(v) FROM pgrc_t; }
step s1bulk { INSERT INTO pgrc_t SELECT g, 1 FROM generate_series(1001, 1040) g; }
step s1del  { DELETE FROM pgrc_t WHERE id % 2 = 0; }
step s1upd  { UPDATE pgrc_t SET v = v + 10 WHERE id % 3 = 0; }
step s1upd7 { UPDATE pgrc_t SET v = 99 WHERE id = 7; }
step s1c    { COMMIT; }

session s2
setup       { SET pgrust.pgrc_bulk_rows = 20; }
step s2bulk { INSERT INTO pgrc_t SELECT g, 2 FROM generate_series(501, 540) g; }
step s2rows { INSERT INTO pgrc_t SELECT g, 2 FROM generate_series(601, 605) g; }
step s2upd7 { UPDATE pgrc_t SET v = 50 WHERE id = 7; }

session m
step mseal  { CALL pgrc_seal('pgrc_t'); }

session r
step ragg   { SELECT count(*), sum(id), sum(v) FROM pgrc_t; }
step rsome  { SELECT id, v FROM pgrc_t WHERE id < 20 OR id BETWEEN 501 AND 506 OR id > 600 AND id < 1010 ORDER BY id; }

# DELETE after the reader's publish chained on another transaction's bulk part
permutation s1rr s1agg s2bulk s1bulk s1del s1agg s1c ragg rsome
# UPDATE after it chained on the flush's seal of rows committed after the snapshot
permutation s1rr s1agg s2rows mseal s1bulk s1upd s1agg s1c ragg rsome
# both, over both
permutation s1rr s1agg s2bulk s2rows mseal s1bulk s1del s1upd s1agg s1c ragg rsome
# SERIALIZABLE
permutation s1ser s1agg s2bulk s2rows mseal s1bulk s1del s1upd s1agg s1c ragg rsome
# a concurrent UPDATE of the row it updates: heap's 40001
permutation s1rr s1agg s2upd7 s2bulk s1bulk s1upd7 s1c ragg rsome
# READ COMMITTED: each statement sees and changes the latest commits
permutation s1rc s1agg s2bulk s2rows mseal s1bulk s1del s1upd s1agg s1c ragg rsome

# D839 (core lane, v5): a REPEATABLE READ or SERIALIZABLE transaction that publishes into a pgrcolumnar table sees its
# snapshot's rows plus its own, never the rows of a generation another transaction committed after its snapshot that
# its publish chained on (PostgreSQL's heap never shows them). s2 COPYs after s1's snapshot; s1 then COPYs its own
# rows. Under READ COMMITTED each statement sees the latest commits. PostgreSQL has no pgrcolumnar: the setup names
# heap's handler as an access method of that name when none exists, so the expected output is heap's.
# D3421: the rows come from a program, never from fixed server-side files: two servers running this spec at once
# (S3 runs W0 and W8 together on one box) truncated each other's /tmp/d839-*.tsv between one's COPY TO and the
# other's COPY FROM.

setup { DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_am WHERE amname = 'pgrcolumnar') THEN CREATE ACCESS METHOD pgrcolumnar TYPE TABLE HANDLER heap_tableam_handler; END IF; END $$; }
setup { CREATE TABLE d839_t (id int, v int) USING pgrcolumnar; }
setup { COPY d839_t FROM PROGRAM 'seq 1 1000 | sed s/$/,0/' WITH (DELIMITER ','); }

teardown { DROP TABLE d839_t; }

session s1
step s1rr   { BEGIN ISOLATION LEVEL REPEATABLE READ; }
step s1ser  { BEGIN ISOLATION LEVEL SERIALIZABLE; }
step s1rc   { BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1agg  { SELECT count(*), sum(id), min(id), max(id), sum(v) FROM d839_t; }
step s1copy { COPY d839_t FROM PROGRAM 'seq 1001 2000 | sed s/$/,1/' WITH (DELIMITER ','); }
step s1c    { COMMIT; }

session s2
step s2copy { COPY d839_t FROM PROGRAM 'seq 5001 6000 | sed s/$/,2/' WITH (DELIMITER ','); }

session r
step ragg   { SELECT count(*), sum(id), min(id), max(id), sum(v) FROM d839_t; }

# another transaction's COPY committed after the snapshot, then the reader's own COPY
permutation s1rr s1agg s2copy s1copy s1agg s1c ragg
# two own COPYs over it
permutation s1rr s1agg s2copy s1copy s1copy s1agg s1c ragg
# SERIALIZABLE
permutation s1ser s1agg s2copy s1copy s1agg s1c ragg
# READ COMMITTED: each statement sees the latest commits
permutation s1rc s1agg s2copy s1copy s1agg s1c ragg

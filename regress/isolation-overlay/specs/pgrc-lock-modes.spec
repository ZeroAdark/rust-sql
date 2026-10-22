# LOCK (pgrc delta-store, 2026-10-03): row locks on a pgrcolumnar table are heap's. PG's tuplelock-conflict on a
# twin: s1 holds FOR KEY SHARE / SHARE / NO KEY UPDATE / UPDATE (after a KEY SHARE and a savepoint, so its lock is a
# multixact, or alone), s2 asks for each strength; s2 waits exactly where heap's does. The rows: id 1 in a sealed
# part, id 11 in the live segment (each permutation locks both). Expected output: the heap twin
# (scripts/pgrc-isolation.sh).

setup { CREATE TABLE pgrc_t (id int, v int) USING pgrcolumnar; }
setup { INSERT INTO pgrc_t VALUES (1, 0); }
setup { CALL pgrc_seal('pgrc_t'); }
setup { INSERT INTO pgrc_t VALUES (11, 0); }

teardown { DROP TABLE pgrc_t; }

session s1
step s1_begin { BEGIN; }
step s1_lcksvpt { SELECT * FROM pgrc_t ORDER BY id FOR KEY SHARE; SAVEPOINT foo; }
step s1_tuplock1 { SELECT * FROM pgrc_t ORDER BY id FOR KEY SHARE; }
step s1_tuplock2 { SELECT * FROM pgrc_t ORDER BY id FOR SHARE; }
step s1_tuplock3 { SELECT * FROM pgrc_t ORDER BY id FOR NO KEY UPDATE; }
step s1_tuplock4 { SELECT * FROM pgrc_t ORDER BY id FOR UPDATE; }
step s1_commit { COMMIT; }

session s2
step s2_tuplock1 { SELECT * FROM pgrc_t ORDER BY id FOR KEY SHARE; }
step s2_tuplock2 { SELECT * FROM pgrc_t ORDER BY id FOR SHARE; }
step s2_tuplock3 { SELECT * FROM pgrc_t ORDER BY id FOR NO KEY UPDATE; }
step s2_tuplock4 { SELECT * FROM pgrc_t ORDER BY id FOR UPDATE; }

permutation s1_begin s1_lcksvpt s1_tuplock1 s2_tuplock1 s1_commit
permutation s1_begin s1_lcksvpt s1_tuplock1 s2_tuplock2 s1_commit
permutation s1_begin s1_lcksvpt s1_tuplock1 s2_tuplock3 s1_commit
permutation s1_begin s1_lcksvpt s1_tuplock1 s2_tuplock4 s1_commit
permutation s1_begin s1_lcksvpt s1_tuplock2 s2_tuplock1 s1_commit
permutation s1_begin s1_lcksvpt s1_tuplock2 s2_tuplock2 s1_commit
permutation s1_begin s1_lcksvpt s1_tuplock2 s2_tuplock3 s1_commit
permutation s1_begin s1_lcksvpt s1_tuplock2 s2_tuplock4 s1_commit
permutation s1_begin s1_lcksvpt s1_tuplock3 s2_tuplock1 s1_commit
permutation s1_begin s1_lcksvpt s1_tuplock3 s2_tuplock2 s1_commit
permutation s1_begin s1_lcksvpt s1_tuplock3 s2_tuplock3 s1_commit
permutation s1_begin s1_lcksvpt s1_tuplock3 s2_tuplock4 s1_commit
permutation s1_begin s1_lcksvpt s1_tuplock4 s2_tuplock1 s1_commit
permutation s1_begin s1_lcksvpt s1_tuplock4 s2_tuplock2 s1_commit
permutation s1_begin s1_lcksvpt s1_tuplock4 s2_tuplock3 s1_commit
permutation s1_begin s1_lcksvpt s1_tuplock4 s2_tuplock4 s1_commit
permutation s1_begin              s1_tuplock1 s2_tuplock1 s1_commit
permutation s1_begin              s1_tuplock1 s2_tuplock2 s1_commit
permutation s1_begin              s1_tuplock1 s2_tuplock3 s1_commit
permutation s1_begin              s1_tuplock1 s2_tuplock4 s1_commit
permutation s1_begin              s1_tuplock2 s2_tuplock1 s1_commit
permutation s1_begin              s1_tuplock2 s2_tuplock2 s1_commit
permutation s1_begin              s1_tuplock2 s2_tuplock3 s1_commit
permutation s1_begin              s1_tuplock2 s2_tuplock4 s1_commit
permutation s1_begin              s1_tuplock3 s2_tuplock1 s1_commit
permutation s1_begin              s1_tuplock3 s2_tuplock3 s1_commit
permutation s1_begin              s1_tuplock4 s2_tuplock1 s1_commit
permutation s1_begin              s1_tuplock4 s2_tuplock4 s1_commit

# TESTMX (PGRCSERVE M3 L4): foreign keys FROM pgrcolumnar tables TO a heap table under concurrency, once Gravity serves
# the RI statements on the pgrc side (NO ACTION's FOR KEY SHARE check query, CASCADE's DELETE). gs_cn (NO ACTION) and
# gs_cc (ON DELETE CASCADE) reference gs_p; their rows are in a sealed part and in the front segment (a COPY into a
# table with a foreign key fires its RI trigger per row, so its rows stay in the segment until sealed).
#   - a COPY into gs_cn holds FOR KEY SHARE on parent 2 until commit: the parent's DELETE waits, then fails (23503)
#   - a parent DELETE cascading into gs_cc waits for a child row locked FOR UPDATE or updated by another session, then
#     deletes the committed version; after an abort, the original
# Expected output: the heap twin (scripts/pgrc-isolation.sh).

setup { CREATE TABLE gs_p (id int PRIMARY KEY); INSERT INTO gs_p SELECT generate_series(1, 3); }
setup { CREATE TABLE gs_cn (id int, pid int REFERENCES gs_p (id)) USING pgrcolumnar; }
setup { COPY gs_cn FROM PROGRAM 'printf "1,1\n3,3\n"' WITH (FORMAT csv); }
setup { CREATE TABLE gs_cc (id int, pid int REFERENCES gs_p (id) ON DELETE CASCADE, v int NOT NULL DEFAULT 0) USING pgrcolumnar; }
setup { COPY gs_cc (id, pid) FROM PROGRAM 'printf "10,1\n11,3\n12,3\n"' WITH (FORMAT csv); }
setup { CALL pgrc_seal('gs_cc'); }
setup { COPY gs_cc (id, pid) FROM PROGRAM 'printf "13,3\n14,1\n"' WITH (FORMAT csv); }

teardown { DROP TABLE gs_cn; DROP TABLE gs_cc; DROP TABLE gs_p; }

session s1
step s1b    { BEGIN; }
step s1copy { COPY gs_cn FROM PROGRAM 'echo 5,2' WITH (FORMAT csv); }
step s1lock { SELECT id, pid FROM gs_cc WHERE pid = 3 ORDER BY id FOR UPDATE; }
step s1upd  { UPDATE gs_cc SET v = v + 1 WHERE id IN (12, 13) RETURNING id, v; }
step s1c    { COMMIT; }
step s1a    { ROLLBACK; }

session s2
step s2b    { BEGIN; }
step s2d2   { DELETE FROM gs_p WHERE id = 2; }
step s2d3   { DELETE FROM gs_p WHERE id = 3; }
step s2d1   { DELETE FROM gs_p WHERE id = 1; }
step s2c    { COMMIT; }

session r
step rsel   { SELECT 'cn' AS t, id, pid, 0 AS v FROM gs_cn UNION ALL SELECT 'cc', id, pid, v FROM gs_cc ORDER BY 1, 2; }
step rpar   { SELECT id FROM gs_p ORDER BY id; }

# NO ACTION: the new child's key-share lock on the parent row holds the DELETE, whose check then finds the child
permutation s1b s1copy s2b s2d2 s1c s2c rsel rpar
# NO ACTION with children in both the part and the segment: no wait, 23503
permutation s2b s2d1 s2c rsel rpar
# CASCADE: the cascaded DELETE waits for the locked child rows, then deletes them
permutation s1b s1lock s2b s2d3 s1c s2c rsel rpar
# CASCADE behind an UPDATE of child rows: committed, then aborted
permutation s1b s1upd s2b s2d3 s1c s2c rsel rpar
permutation s1b s1upd s2b s2d3 s1a s2c rsel rpar

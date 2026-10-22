# TESTMX (PGRCSERVE M3 L4): a cross-partition UPDATE between pgrcolumnar leaves under concurrency, once Gravity serves
# pgrc UPDATE with row movement. Row 7 moves from gs_pt_a to gs_pt_b; a concurrent UPDATE, DELETE or FOR UPDATE of
# it waits, then fails with 40001 ("already moved to another partition"); after the mover's abort it acts on the
# original. Seeded by COPY (routed into the leaves). Expected output: the heap twin (scripts/pgrc-isolation.sh).

setup { CREATE TABLE gs_pt (id int, v int NOT NULL DEFAULT 0) PARTITION BY RANGE (id); }
setup { CREATE TABLE gs_pt_a PARTITION OF gs_pt FOR VALUES FROM (0) TO (100) USING pgrcolumnar; }
setup { CREATE TABLE gs_pt_b PARTITION OF gs_pt FOR VALUES FROM (100) TO (200) USING pgrcolumnar; }
setup { COPY gs_pt (id) FROM PROGRAM 'seq 1 10'; }

teardown { DROP TABLE gs_pt; }

session s1
step s1b    { BEGIN; }
step s1mv   { UPDATE gs_pt SET id = id + 100 WHERE id = 7 RETURNING tableoid::regclass, id, v; }
step s1c    { COMMIT; }
step s1a    { ROLLBACK; }

session s2
step s2b    { BEGIN; }
step s2u    { UPDATE gs_pt SET v = v + 1 WHERE id = 7 RETURNING tableoid::regclass, id, v; }
step s2d    { DELETE FROM gs_pt WHERE id = 7 RETURNING tableoid::regclass, id; }
step s2lk   { SELECT tableoid::regclass, id FROM gs_pt WHERE id = 7 FOR UPDATE; }
step s2c    { COMMIT; }

session r
step rsel   { SELECT tableoid::regclass, id, v FROM gs_pt WHERE id IN (7, 107) ORDER BY id; }

permutation s1b s1mv s2b s2u s1c s2c rsel
permutation s1b s1mv s2b s2d s1c s2c rsel
permutation s1b s1mv s2b s2lk s1c s2c rsel
permutation s1b s1mv s2b s2u s1a s2c rsel

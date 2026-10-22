# D882: DELETE .. USING whose target the planner hashes on a join's build side, the emit scanning a REFERENCE-marked
# relation (write.lock's DELETE end, the row op at the target's junk ctid). A concurrent UPDATE of the target row that
# keeps it in the qual (the recheck deletes the newest version), one that moves it out of the qual or off the join key
# (the recheck skips it), a concurrent DELETE of it (skipped), a concurrent UPDATE of the joined row (no lock: no
# recheck); the same with RETURNING.
setup
{
 CREATE TABLE ep_t (id int PRIMARY KEY, v int);
 CREATE TABLE ep_b (id int PRIMARY KEY, n int);
 INSERT INTO ep_t VALUES (1, 10), (2, 20), (3, 30);
 INSERT INTO ep_b SELECT g, g FROM generate_series(1, 100) g;
 ANALYZE ep_t;
 ANALYZE ep_b;
}
teardown { DROP TABLE ep_t; DROP TABLE ep_b; }
session s1
setup { BEGIN ISOLATION LEVEL READ COMMITTED; SET enable_nestloop = off; SET enable_mergejoin = off; }
step d { DELETE FROM ep_t USING ep_b b WHERE ep_t.id = b.id AND ep_t.v < 100; }
step dr { DELETE FROM ep_t USING ep_b b WHERE ep_t.id = b.id AND ep_t.v < 100 RETURNING ep_t.*; }
step c1 { COMMIT; }
session s2
setup { BEGIN ISOLATION LEVEL READ COMMITTED; }
step u { UPDATE ep_t SET v = v + 1 WHERE id = 2; }
step uo { UPDATE ep_t SET v = v + 500 WHERE id = 2; }
step uk { UPDATE ep_t SET id = id + 1000 WHERE id = 2; }
step x { DELETE FROM ep_t WHERE id = 2; }
step ub { UPDATE ep_b SET n = n + 1 WHERE id = 2; }
step c2 { COMMIT; }
session s3
step read { SELECT * FROM ep_t ORDER BY id; }
permutation u d c2 c1 read
permutation uo d c2 c1 read
permutation uk d c2 c1 read
permutation x d c2 c1 read
permutation ub d c2 c1 read
permutation u dr c2 c1 read
permutation uo dr c2 c1 read
permutation x dr c2 c1 read

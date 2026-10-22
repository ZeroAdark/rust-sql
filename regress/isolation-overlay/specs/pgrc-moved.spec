# pgrc F2 L5 (docs/design/pgrc-delta-v2/F2.md): cross-partition UPDATE of a
# row of a pgrcolumnar leaf against a concurrent UPDATE or DELETE of the same
# row. The waiter fails with 40001 and heap's message ("tuple to be locked was
# already moved to another partition due to concurrent update"). Leaves p1 and
# p2 are pgrcolumnar with one sealed part (ids 1-10) and one live segment (ids
# 11-20) each; the targets are id 1 and id 11 of p1. Expected output: the heap
# twin (scripts/pgrc-isolation.sh).

setup
{
  CREATE TABLE pgrc_p (k int, id int, v int) PARTITION BY LIST (k);
  CREATE TABLE pgrc_p1 PARTITION OF pgrc_p FOR VALUES IN (1) USING pgrcolumnar;
  CREATE TABLE pgrc_p2 PARTITION OF pgrc_p FOR VALUES IN (2) USING pgrcolumnar;
}
setup { INSERT INTO pgrc_p SELECT k, g, 0 FROM generate_series(1, 10) g, generate_series(1, 2) k; }
setup { CALL pgrc_seal('pgrc_p1'); }
setup { CALL pgrc_seal('pgrc_p2'); }
setup { INSERT INTO pgrc_p SELECT k, g, 0 FROM generate_series(11, 20) g, generate_series(1, 2) k; }

teardown { DROP TABLE pgrc_p; }

session s1
step s1b    { BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1mp   { UPDATE pgrc_p SET k = 2 WHERE k = 1 AND id = 1 RETURNING k, id, v; }
step s1ms   { UPDATE pgrc_p SET k = 2 WHERE k = 1 AND id = 11 RETURNING k, id, v; }
step s1c    { COMMIT; }
step s1a    { ROLLBACK; }

session s2
step s2b    { BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2up   { UPDATE pgrc_p SET v = v + 10 WHERE k = 1 AND id = 1 RETURNING k, id, v; }
step s2us   { UPDATE pgrc_p SET v = v + 10 WHERE k = 1 AND id = 11 RETURNING k, id, v; }
step s2ds   { DELETE FROM pgrc_p WHERE k = 1 AND id = 11 RETURNING k, id, v; }
step s2mp   { UPDATE pgrc_p SET k = 2, v = v + 100 WHERE k = 1 AND id = 1 RETURNING k, id, v; }
step s2c    { COMMIT; }

session r
step rsel   { SELECT k, id, v FROM pgrc_p WHERE id IN (1, 11) ORDER BY k, id; }
step rcount { SELECT tableoid::regclass, count(*) FROM pgrc_p GROUP BY 1 ORDER BY 1; }

permutation s1b s1mp s2b s2up s1c s2c rsel rcount
permutation s1b s1ms s2b s2us s1c s2c rsel rcount
permutation s1b s1ms s2b s2ds s1c s2c rsel rcount
permutation s1b s1mp s2b s2mp s1c s2c rsel rcount
# the mover aborts: the waiter acts on the original row
permutation s1b s1ms s2b s2us s1a s2c rsel rcount

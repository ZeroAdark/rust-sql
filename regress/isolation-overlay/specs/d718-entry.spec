# D718: a multi-row SELECT .. FOR UPDATE sent as one simple query of two statements ("SET ..; SELECT .. FOR UPDATE",
# how clients send it) runs through the entry seam; it waits on a row a concurrent transaction updated, rechecks it
# after that transaction ends (EvalPlanQual) and goes on to the rows after it. lkq (a, b), rows 0..29, b = a % 4;
# B updates row 13 and holds; A locks a > 11 AND a < 16: PostgreSQL 12|0 13|5 14|2 15|3 when B's update keeps the row
# in A's qual, 12|0 14|2 15|3 when B moves it out (a = 113), 12|0 13|1 14|2 15|3 when B rolls back. Trunk 77a6ea67855
# ended the SELECT after the rechecked row. Session ei reads an index range (Index Scan using lkq_pkey), es a seq
# range; permutation pei pes (last) pins both plans. Session one sends the SELECT alone (the runner path, right on trunk).
setup
{
 CREATE TABLE lkq (a int PRIMARY KEY, b int);
 INSERT INTO lkq SELECT g, g % 4 FROM generate_series(0, 29) g;
 ANALYZE lkq;
}
teardown { DROP TABLE lkq; }

session b
setup { BEGIN ISOLATION LEVEL READ COMMITTED; }
step b13 { UPDATE lkq SET b = 5 WHERE a = 13; }
step bx  { UPDATE lkq SET a = 113 WHERE a = 13; }
step bc  { COMMIT; }
step br  { ROLLBACK; }

session ei
setup { SET enable_bitmapscan = off; }
step pei { SET enable_seqscan = off; EXPLAIN (COSTS OFF) SELECT a, b FROM lkq WHERE a > 11 AND a < 16 FOR UPDATE; }
step ei  { SET enable_seqscan = off; SELECT a, b FROM lkq WHERE a > 11 AND a < 16 FOR UPDATE; }

session es
setup { SET enable_bitmapscan = off; }
step pes { SET enable_indexscan = off; EXPLAIN (COSTS OFF) SELECT a, b FROM lkq WHERE a > 11 AND a < 16 FOR UPDATE; }
step es  { SET enable_indexscan = off; SELECT a, b FROM lkq WHERE a > 11 AND a < 16 FOR UPDATE; }

session one
setup { SET enable_seqscan = off; SET enable_bitmapscan = off; }
step one { SELECT a, b FROM lkq WHERE a > 11 AND a < 16 FOR UPDATE; }

permutation b13 ei bc
permutation bx ei bc
permutation b13 ei br
permutation b13 es bc
permutation bx es bc
permutation b13 one bc
permutation pei pes

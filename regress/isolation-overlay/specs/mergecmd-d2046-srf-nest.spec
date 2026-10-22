# mergecmd D2046-SRF (gen mergecmd source-generate-series's form, mg-0015, over the target's primary key index): a MERGE
# whose source is a FROM function (generate_series: its row a COPY mark) and whose target is a nested loop's inner index
# scan, while another session changes a matched row (READ COMMITTED). PostgreSQL 18.6's ExecMergeMatched rechecks the
# newest version under EvalPlanQual with the function's row handed over whole (the function is never called again) and
# the inner index scan returning that version alone. s2 turns hash and merge joins off and s2exp pins the plan (Nested
# Loop Left Join, the Function Scan outer, the target's primary key index scan inner).
#  (a) s1 updates a matched row: the UPDATE reads the newest version
#  (b) s1 moves the row's key away: the recheck finds no match, the NOT MATCHED INSERT runs for that series value
#  (c) s1 deletes the row: the NOT MATCHED INSERT runs
#  (d) s1 makes the WHEN qual false on the newest version: the next action (DO NOTHING) applies
#  (e) s1 aborts: the original version is updated (control)
# Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE sr_t (id int PRIMARY KEY, n int NOT NULL, val int NOT NULL, hits int NOT NULL DEFAULT 0);
 INSERT INTO sr_t SELECT i, i, (i * 37) % 1000, 0 FROM generate_series(1, 2000) i;
 ANALYZE sr_t;
}

teardown
{
 DROP TABLE sr_t;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1upd	{ UPDATE sr_t SET hits = hits + 10, val = val + 1 WHERE id = 1998; }
step s1key	{ UPDATE sr_t SET id = 1998000 WHERE id = 1998; }
step s1del	{ DELETE FROM sr_t WHERE id = 1998; }
step s1qual	{ UPDATE sr_t SET hits = 100 WHERE id = 1998; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; }
step s2exp	{ EXPLAIN (COSTS OFF) MERGE INTO sr_t t USING generate_series(1995, 2003) g ON t.id = g WHEN MATCHED AND t.hits < 50 THEN UPDATE SET hits = t.hits + 1, val = t.val + g WHEN MATCHED THEN DO NOTHING WHEN NOT MATCHED THEN INSERT (id, n, val) VALUES (g, -g, 0); }
step s2mrg	{ MERGE INTO sr_t t USING generate_series(1995, 2003) g ON t.id = g WHEN MATCHED AND t.hits < 50 THEN UPDATE SET hits = t.hits + 1, val = t.val + g WHEN MATCHED THEN DO NOTHING WHEN NOT MATCHED THEN INSERT (id, n, val) VALUES (g, -g, 0); }
step s2sel	{ SELECT id, n, val, hits FROM sr_t WHERE id >= 1995 ORDER BY id; }

permutation s2exp s1upd s2mrg s1c s2sel
permutation s1key s2mrg s1c s2sel
permutation s1del s2mrg s1c s2sel
permutation s1qual s2mrg s1c s2sel
permutation s1upd s2mrg s1a s2sel

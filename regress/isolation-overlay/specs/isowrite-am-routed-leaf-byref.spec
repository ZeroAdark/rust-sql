# D3383: the routed INSERT of isowrite-am-routed-leaf (its leaf given the pgrcolumnar table AM by a concurrent ALTER TABLE
# .. SET ACCESS METHOD that commits between the INSERT's admission and its first row into the leaf), now with rows of
# varied by-reference width. hosts write.rs insert_via_am stores each row in the leaf's virtual slot and the AM
# materializes it under the per-row exec context, which row_op resets after every row. The slot kept its materialize
# buffer across rows, so a row wide enough to need a bump block past the context's 8 kB keeper left that buffer in a
# block the reset recycled or freed, and a later narrower row was materialized into it: the backend crashed (SIGSEGV,
# row bytes over a pointer) at W0 and W8. isowrite-am-routed-leaf's two short rows fit the keeper and never showed it.
# In PostgreSQL the AM is heap's handler under another name: every row stands in the leaf, byte for byte.
setup
{
 DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_am WHERE amname = 'pgrcolumnar') THEN CREATE ACCESS METHOD pgrcolumnar TYPE TABLE HANDLER heap_tableam_handler; END IF; END $$;
 CREATE TABLE rp (a int, b text) PARTITION BY LIST (a);
 CREATE TABLE rp1 PARTITION OF rp FOR VALUES IN (1);
 CREATE TABLE rp2 PARTITION OF rp FOR VALUES IN (2);
}
teardown
{
 DROP TABLE rp;
 DROP ACCESS METHOD IF EXISTS pgrcolumnar;
}
session s1
step s1b	{ BEGIN; }
step s1am	{ ALTER TABLE rp1 SET ACCESS METHOD pgrcolumnar; }
step s1c	{ COMMIT; }
session s2
step s2w8	{ INSERT INTO rp SELECT 1, repeat(chr(97 + g % 26), (g * 7919) % 30000 + 1) FROM generate_series(1, 8) g; }
step s2w40	{ INSERT INTO rp SELECT 1, repeat(chr(97 + g % 26), (g * 7919) % 30000 + 1) FROM generate_series(1, 40) g; }
session s3
step s3s	{ SELECT tableoid::regclass, a, length(b), md5(b) FROM rp ORDER BY 1, 2, 3, 4; }
permutation s1b s1am s2w8 s1c s3s
permutation s1b s1am s2w40 s1c s3s

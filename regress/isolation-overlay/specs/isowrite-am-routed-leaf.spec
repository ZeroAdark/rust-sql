# D3293: a routed INSERT whose leaf is given the pgrcolumnar table AM by a concurrent ALTER TABLE .. SET ACCESS METHOD that
# commits between the INSERT's admission (every leaf a heap table) and its first row into that leaf. The leaf is opened
# at that row and written through its table AM (hosts write.rs insert_via_am), which buffers the row until
# table_finish_bulk_insert; the statement's end now finishes it (trigger_stmt.rs finish_am). Before, the rows were missing
# inside the transaction and its commit failed ('buffered ingest .. reached commit without table_finish_bulk_insert').
# In PostgreSQL the AM is heap's handler under another name: the rows stand in the leaf, in the transaction and after.
# A write admitted after the commit sees the other-AM leaf and is refused by name (a partition that is not a heap table),
# so each permutation writes only through the INSERT admitted before it.
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
step s2i	{ INSERT INTO rp VALUES (1, 'x'); }
step s2b	{ BEGIN; }
step s2j	{ INSERT INTO rp SELECT g % 2 + 1, 'y' || g FROM generate_series(1, 4) g; }
step s2s	{ SELECT tableoid::regclass, * FROM rp ORDER BY a, b; }
step s2c	{ COMMIT; }
session s3
step s3s	{ SELECT tableoid::regclass, * FROM rp ORDER BY a, b; }
permutation s1b s1am s2i s1c s3s
permutation s1b s1am s2b s2j s1c s2s s2c s3s

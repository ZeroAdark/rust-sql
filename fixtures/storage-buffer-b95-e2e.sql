-- bugs/batch-95-backend-storage-buffer: SQL-visible fixes in storage/buffer,
-- expected captured from C 18.6.
\set VERBOSITY verbose
-- buf_init.c:77-101 BufferManagerShmemInit, buf_table.c:62 InitBufTable,
-- freelist.c:494 StrategyInitialize: the buffer pool's six ShmemIndex rows
-- (sizes at the default shared_buffers = 128MB, NBuffers = 16384).
SHOW shared_buffers;
SELECT name, size, allocated_size FROM pg_shmem_allocations
 WHERE name IN ('Buffer Descriptors', 'Buffer Blocks', 'Buffer IO Condition Variables',
                'Checkpoint BufferIds', 'Shared Buffer Lookup Table', 'Buffer Strategy Status')
 ORDER BY name;
-- bufmgr.c:1157 PinBufferForBlock / heapam.c:1316 read_stream_end: blocks a
-- finished sequential scan read ahead are ordinary buffer hits for the next
-- statement of the same transaction.
CREATE EXTENSION pg_buffercache;
CREATE TABLE b95_t AS SELECT g AS a, repeat('x', 500) AS b FROM generate_series(1, 2000) g;
CHECKPOINT;
SELECT buffers_evicted > 0 AS evicted FROM pg_buffercache_evict_relation('b95_t');
SET io_combine_limit = '128kB';
\o /dev/null
EXPLAIN (ANALYZE, BUFFERS, COSTS OFF, TIMING OFF, SUMMARY OFF) SELECT a FROM b95_t WHERE ctid = '(9,1)'::tid;
\o
SELECT buffers_evicted > 0 AS evicted FROM pg_buffercache_evict_relation('b95_t');
BEGIN;
SELECT a FROM b95_t LIMIT 1;
EXPLAIN (ANALYZE, BUFFERS, COSTS OFF, TIMING OFF, SUMMARY OFF) SELECT a FROM b95_t WHERE ctid = '(1,1)'::tid;
EXPLAIN (ANALYZE, BUFFERS, COSTS OFF, TIMING OFF, SUMMARY OFF) SELECT a FROM b95_t WHERE ctid = '(2,1)'::tid;
COMMIT;
DROP TABLE b95_t;
DROP EXTENSION pg_buffercache;

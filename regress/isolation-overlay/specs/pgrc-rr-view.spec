# pgrc RR-VIEW (pgrc-plans/RR-VIEW-design.md): a REPEATABLE READ or SERIALIZABLE transaction that publishes into a
# table sees its snapshot's rows plus its own, never the rows of a generation committed after its snapshot that its
# publish chained on. s2 publishes after s1's snapshot: a bulk part (s2bulk), or front rows that m's flush seals into a
# part (s2rows mseal: the front worker's seal, forced). s1 then bulk-loads its own part, which chains on that
# generation. Heap never shows s1 the other transaction's rows. Under READ COMMITTED each statement sees the latest
# commits. The table holds parts only, so a reader of the parts alone (Gravity's native road, SQE) would serve it: the
# reader must read the composed view.
# PGRCSERVE M0: the writes are COPYs, so the spec holds before pgrc INSERT (M1). A plain COPY loads the bulk lane (a
# part publish at statement end); a COPY whose WHERE is volatile (copyfrom.c CIM_SINGLE) puts its rows in the front's
# segment, as a short INSERT did. The heap twin runs the same COPYs.
# Expected output: the heap twin (scripts/pgrc-isolation.sh).

setup { CREATE TABLE pgrc_t (id int, v int) USING pgrcolumnar; }
setup { COPY pgrc_t FROM PROGRAM 'seq -f %g,0 1 10' WITH (FORMAT csv); }
setup { CALL pgrc_seal('pgrc_t'); }

teardown { DROP TABLE pgrc_t; }

session s1
step s1rr   { BEGIN ISOLATION LEVEL REPEATABLE READ; }
step s1ser  { BEGIN ISOLATION LEVEL SERIALIZABLE; }
step s1rc   { BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1agg  { SELECT count(*), sum(id), min(id), max(id), sum(v) FROM pgrc_t; }
step s1bulk { COPY pgrc_t FROM PROGRAM 'seq -f %g,1 1001 1040' WITH (FORMAT csv); }
step s1bulk2 { COPY pgrc_t FROM PROGRAM 'seq -f %g,1 2001 2030' WITH (FORMAT csv); }
step s1c    { COMMIT; }

session s2
step s2bulk { COPY pgrc_t FROM PROGRAM 'seq -f %g,2 501 540' WITH (FORMAT csv); }
step s2rows { COPY pgrc_t FROM PROGRAM 'seq -f %g,2 601 605' WITH (FORMAT csv) WHERE random() >= 0; }

session m
step mseal  { CALL pgrc_seal('pgrc_t'); }

session r
step ragg   { SELECT count(*), sum(id), min(id), max(id), sum(v) FROM pgrc_t; }

# (a) another transaction's bulk part, then the reader's own
permutation s1rr s1agg s2bulk s1bulk s1agg s1c ragg
# (a) two own publishes over it
permutation s1rr s1agg s2bulk s1bulk s1bulk2 s1agg s1c ragg
# (b) rows committed after the snapshot, sealed by the flush, then the reader's own bulk part
permutation s1rr s1agg s2rows mseal s1bulk s1agg s1c ragg
# (b) both: a bulk part and a seal after the snapshot
permutation s1rr s1agg s2bulk s2rows mseal s1bulk s1agg s1bulk2 s1agg s1c ragg
# (c) SERIALIZABLE
permutation s1ser s1agg s2bulk s1bulk s1agg s1c ragg
permutation s1ser s1agg s2rows mseal s1bulk s1agg s1c ragg
# (e) READ COMMITTED: each statement sees the latest commits
permutation s1rc s1agg s2bulk s1bulk s1agg s1c ragg
permutation s1rc s1agg s2rows mseal s1bulk s1agg s1c ragg

# pgrc F2 L5 (docs/design/pgrc-delta-v2/F2.md): pgrc_flush in one session
# between another's scan and its UPDATE. The target is a live-segment row (id
# 11), which the flush seals into a part under the updater; the UPDATE must
# change it exactly once (one row, one new version, count unchanged). s1 scans
# either in an earlier statement of its transaction or in the UPDATE itself,
# held in its SET list by pgrc_gate (advisory lock 1, held by session c).
# Expected output: the heap twin (scripts/pgrc-isolation.sh), where the flush
# is a no-op.
#
# fix1: a REPEATABLE READ or SERIALIZABLE snapshot taken before the flush reads
# the generation that predates it, while the transaction's own writes after it
# (UPDATE's new version, an INSERT) land in the segment the flush activated.
# The transaction must still see them (design §4.8), and its own later
# tombstones on them (a second UPDATE, a DELETE) must apply. The SERIALIZABLE
# permutation fails on the heap twin (D551), so the expected file is the twin's
# output on PostgreSQL 18 (as L5's overrides).

setup { CREATE TABLE pgrc_t (id int, v int) USING pgrcolumnar; }
setup { INSERT INTO pgrc_t SELECT g, 0 FROM generate_series(1, 10) g; }
setup { CALL pgrc_seal('pgrc_t'); }
setup { INSERT INTO pgrc_t SELECT g, 0 FROM generate_series(11, 20) g; }
setup
{
  CREATE FUNCTION pgrc_gate(k int, v int) RETURNS int LANGUAGE sql VOLATILE
    AS 'SELECT pg_advisory_xact_lock_shared(k); SELECT v';
}

teardown
{
  DROP TABLE pgrc_t;
  DROP FUNCTION pgrc_gate(int, int);
}

session c
step chold  { SELECT pg_advisory_lock(1); }
step cgo    { SELECT pg_advisory_unlock(1); }

session s1
step s1rc   { BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1rr   { BEGIN ISOLATION LEVEL REPEATABLE READ; }
step s1sr   { BEGIN ISOLATION LEVEL SERIALIZABLE; }
step s1scan { SELECT id, v FROM pgrc_t WHERE id IN (1, 11) ORDER BY id; }
step s1u    { UPDATE pgrc_t SET v = v + 1 WHERE id = 11 RETURNING id, v; }
step s1ug   { UPDATE pgrc_t SET v = pgrc_gate(1, v + 1) WHERE id = 11 RETURNING id, v; }
step s1i    { INSERT INTO pgrc_t VALUES (21, 5); }
step s1d    { DELETE FROM pgrc_t WHERE id = 21 RETURNING id, v; }
step s1all  { SELECT count(*), sum(v), min(id), max(id) FROM pgrc_t; }
step s1c    { COMMIT; }

session s2
step s2seal { CALL pgrc_seal('pgrc_t'); }

session r
step rsel   { SELECT id, v FROM pgrc_t WHERE id IN (1, 11) ORDER BY id; }
step rcount { SELECT count(*), sum(v), count(DISTINCT id) FROM pgrc_t; }

# the scan is an earlier statement
permutation s1rc s1scan s2seal s1u s1c rsel rcount
permutation s1rr s1scan s2seal s1u s1scan s1c rsel rcount
# the scan is the UPDATE's own, held before the write
permutation chold s1rc s1ug s2seal cgo s1c rsel rcount
permutation chold s1rr s1scan s1ug s2seal cgo s1scan s1c rsel rcount
# the seal runs while the updater is uncommitted
permutation s1rc s1u s2seal s1c rsel rcount
# fix1: own writes into the segment the flush activated, under a snapshot that
# predates it (the updated row was lost); then own tombstones on them
permutation s1rr s1scan s2seal s1u s1scan s1all s1c rsel rcount
permutation s1sr s1scan s2seal s1u s1scan s1c rsel rcount
permutation s1rr s1scan s2seal s1u s1u s1scan s1c rsel rcount
permutation s1rr s1all s2seal s1i s1all s1d s1all s1c rsel rcount
permutation s1rc s1scan s2seal s1u s1u s1i s1all s1c rsel rcount

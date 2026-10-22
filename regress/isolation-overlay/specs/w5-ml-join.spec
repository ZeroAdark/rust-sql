# W5 multilock Row A (lowerv4 write/lock_marks.rs): SELECT .. FOR UPDATE over a hash join (b on the build side, the plan
# pinned by the EXPLAIN steps), EvalPlanQual over every locked relation. ExecLockRows locks a's row, then b's, and runs
# EvalPlanQual once when a lock followed an update chain. s1 changes a row between s2's read and s2's lock:
# - a's v updated: the EPQ row has the new a;
# - b's w updated: the build side's locked relation traversed (its producer reruns at the newest version);
# - both updated: two traversed, one EPQ;
# - b's a_id moved away: the join fails, the row is skipped;
# - b's row deleted (TM_Deleted) or a's: skipped;
# - a's v set to -1: the filter fails.
# s2's FOR UPDATE OF b only is a lock on the build side: a is a REFERENCE row, read at the version the join read (s1's
# update of a neither blocks s2 nor shows). Under REPEATABLE READ and SERIALIZABLE (s3), TM_Updated and TM_Deleted raise
# 40001 on either mark. Expected output is the C oracle's (PostgreSQL 18, pgdg).

setup
{
  CREATE TABLE ma (id int PRIMARY KEY, v int NOT NULL);
  INSERT INTO ma SELECT i, i FROM generate_series(1, 10) i;
  CREATE TABLE mb (id int PRIMARY KEY, a_id int NOT NULL, w int NOT NULL);
  INSERT INTO mb SELECT i, i, i * 10 FROM generate_series(1, 10) i;
  ANALYZE ma, mb;
}

teardown
{
  DROP TABLE mb;
  DROP TABLE ma;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1a	{ UPDATE ma SET v = v + 100 WHERE id = 2; }
step s1b	{ UPDATE mb SET w = w + 1 WHERE id = 2; }
step s1ab	{ UPDATE ma SET v = v + 100 WHERE id = 3; UPDATE mb SET w = w + 1 WHERE id = 3; }
step s1mv	{ UPDATE mb SET a_id = 9 WHERE id = 2; }
step s1db	{ DELETE FROM mb WHERE id = 2; }
step s1da	{ DELETE FROM ma WHERE id = 2; }
step s1neg	{ UPDATE ma SET v = -1 WHERE id = 2; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2b	{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT a.id, a.v, b.id, b.w FROM ma a JOIN mb b ON b.a_id = a.id WHERE a.v > 0 AND b.w < 50 FOR UPDATE; }
step s2j	{ SELECT a.id, a.v, b.id, b.w FROM ma a JOIN mb b ON b.a_id = a.id WHERE a.v > 0 AND b.w < 50 FOR UPDATE; }
step s2xob	{ EXPLAIN (COSTS OFF) SELECT a.id, a.v, b.id, b.w FROM ma a JOIN mb b ON b.a_id = a.id WHERE a.v > 0 AND b.w < 50 FOR UPDATE OF b; }
step s2ob	{ SELECT a.id, a.v, b.id, b.w FROM ma a JOIN mb b ON b.a_id = a.id WHERE a.v > 0 AND b.w < 50 FOR UPDATE OF b; }
step s2c	{ COMMIT; }

session s3
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s3rr	{ BEGIN ISOLATION LEVEL REPEATABLE READ; }
step s3sr	{ BEGIN ISOLATION LEVEL SERIALIZABLE; }
step s3snap	{ SELECT count(*) FROM ma; }
step s3j	{ SELECT a.id, a.v, b.id, b.w FROM ma a JOIN mb b ON b.a_id = a.id WHERE a.v > 0 AND b.w < 50 FOR UPDATE; }
step s3c	{ COMMIT; }

session s4
step s4sel	{ SELECT id, v FROM ma WHERE id <= 4 ORDER BY id; SELECT id, a_id, w FROM mb WHERE id <= 4 ORDER BY id; }

# a's row updated: the EPQ row carries a's new v (102)
permutation s2b s2x s1a s2j s1c s2c s4sel
# b's row (the build side) updated: its producer reruns at the newest version (w 21)
permutation s2b s1b s2j s1c s2c
# both updated: one EPQ over both new versions
permutation s2b s1ab s2j s1c s2c
# b's a_id moved away: the join fails in the EPQ, the row is skipped (its locks kept)
permutation s2b s1mv s2j s1c s2c
# b's row deleted, then a's: skipped
permutation s2b s1db s2j s1c s2c
permutation s2b s1da s2j s1c s2c
# a's v fails the filter in the EPQ: skipped
permutation s2b s1neg s2j s1c s2c
# FOR UPDATE OF b: b's update rechecks (a read at the version the join read); a's update neither blocks nor shows
permutation s2b s2xob s1b s2ob s1c s2c
permutation s2b s1a s2ob s1c s2c
permutation s2b s1mv s2ob s1c s2c
# REPEATABLE READ / SERIALIZABLE: TM_Updated and TM_Deleted on either mark raise 40001
permutation s3rr s3snap s1a s3j s1c s3c
permutation s3rr s3snap s1db s3j s1c s3c
permutation s3sr s3snap s1b s3j s1c s3c
permutation s3sr s3snap s1da s3j s1c s3c

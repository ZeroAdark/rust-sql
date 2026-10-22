# pglguc C4: LockReleaseAll walks the 16 lock partitions and skips each one this backend holds no lock in before it looks
# up the partition's LWLock (the lookup moved past the skip). s1 holds a strong relation lock (main lock table, one
# partition), a transaction advisory lock and a session advisory lock (the user lock method, which a commit keeps); the
# partitions it holds nothing in are skipped. At s1's commit the relation lock and the transaction advisory lock go and
# their waiters (s2, s3) run; the session advisory lock stays (s4 cannot take it) until s1 unlocks it. s4 holds no lock
# in any partition of its own until it takes the advisory lock, and its commit-time walks skip them all.
setup
{
 CREATE TABLE lrskip (id int);
 INSERT INTO lrskip VALUES (1), (2), (3);
}

teardown
{
 DROP TABLE lrskip;
}

session s1
step s1b	{ BEGIN; }
step s1l	{ LOCK TABLE lrskip IN ACCESS EXCLUSIVE MODE; }
step s1x	{ SELECT pg_advisory_xact_lock(4301); }
step s1a	{ SELECT pg_advisory_lock(4302); }
step s1c	{ COMMIT; }
step s1u	{ SELECT pg_advisory_unlock(4302); }

session s2
step s2r	{ SELECT count(*) FROM lrskip; }

session s3
step s3x	{ SELECT pg_advisory_xact_lock(4301); }

session s4
step s4t	{ SELECT pg_try_advisory_lock(4302); }
step s4u	{ SELECT pg_advisory_unlock(4302); }
step s4n	{ SELECT count(*) FROM pg_locks WHERE locktype = 'relation' AND relation = 'lrskip'::regclass; }

permutation s1b s1l s1x s1a s2r s3x s4n s1c s4t s4n s1u s4t s4u

# lockrows lk-0100 (D2131): SELECT .. WHERE EXISTS (..) .. FOR UPDATE planned as LockRows over a Hash Right Semi Join (the
# locked relation ia hashed, il probing; il carries a REFERENCE mark). Gravity spools the probe's first-match rows and its
# drain hands them to the lock. EvalPlanQual reruns the join over the locked row's newest version and the one il row
# the statement joined it with (its junk ctid, at the version read), never re-scanning il.
# Permutation 1: s1 bumps ia's row 2; s2's lock waits, then rechecks over the new version: v 102.
# Permutation 2: s1 moves ia's row 2 to id 7 (free, still under a.id < 10, and il has an amt -37 row for acct 7): the recheck
# joins the new version with the il row it read (acct 2): no match, the row is dropped.
# Permutation 3: s1 moves it to id 9000: the new version fails a.id < 10, dropped.
# Permutation 4: SKIP LOCKED passes over the row s1 holds, without waiting.
# Permutation 5: NOWAIT raises on it.
# D3440: the drain stays spooled in a statement that locks rows; rxcursor#2104's streamed RIGHT SEMI drain (D1922) took it
# here and its recheck handed on the row before the rechecked one (row 1 twice, permutations 1 to 3).
setup
{
  CREATE TABLE ia (id int PRIMARY KEY, v int) WITH (autovacuum_enabled = off);
  CREATE TABLE il (id int PRIMARY KEY, acct int NOT NULL, amt int NOT NULL) WITH (autovacuum_enabled = off);
  INSERT INTO ia SELECT i, i FROM generate_series(1, 3000) i WHERE i NOT BETWEEN 6 AND 10;
  INSERT INTO il SELECT i, 1 + (i / 200) % 3000, (i % 200) - 100 FROM generate_series(1, 30000) i;
  ANALYZE ia;
  ANALYZE il;
}

teardown
{
  DROP TABLE ia, il;
}

session s1
step s1b { BEGIN; }
step s1bump { UPDATE ia SET v = v + 100 WHERE id = 2; }
step s1move { UPDATE ia SET id = 7 WHERE id = 2; }
step s1out { UPDATE ia SET id = 9000 WHERE id = 2; }
step s1hold { SELECT id FROM ia WHERE id = 2 FOR UPDATE; }
step s1c { COMMIT; }

session s2
setup { SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2lock { SELECT a.id, a.v FROM ia a WHERE EXISTS (SELECT 1 FROM il l WHERE l.acct = a.id AND l.amt = -37) AND a.id < 10 FOR UPDATE; }
step s2skip { SELECT a.id, a.v FROM ia a WHERE EXISTS (SELECT 1 FROM il l WHERE l.acct = a.id AND l.amt = -37) AND a.id < 10 FOR UPDATE SKIP LOCKED; }
step s2nowait { SELECT a.id, a.v FROM ia a WHERE EXISTS (SELECT 1 FROM il l WHERE l.acct = a.id AND l.amt = -37) AND a.id < 10 FOR UPDATE NOWAIT; }

permutation s1b s1bump s2lock s1c
permutation s1b s1move s2lock s1c
permutation s1b s1out s2lock s1c
permutation s1b s1hold s2skip s1c
permutation s1b s1hold s2nowait s1c

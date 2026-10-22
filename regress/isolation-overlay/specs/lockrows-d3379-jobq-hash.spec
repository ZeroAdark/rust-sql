# D3379 JOBQ HASH (m4_iso's claim): UPDATE / DELETE jqh .. WHERE id IN (SELECT id FROM jqh WHERE state = 'new' ORDER BY id
# LIMIT 2 FOR UPDATE [SKIP LOCKED]) over a small table plans a Hash Semi Join (the target's scan probes Hash -> Subquery
# Scan -> Limit -> LockRows -> Sort -> Seq Scan). The lock rechecks in the IR over the Sort's producer (15b); the write's
# recheck reads the subquery's row from its ROW_MARK_COPY mark and never locks again.
# Permutation 1: s1's claim holds rows 1 and 2; s2's SKIP LOCKED claim passes over them: 3 and 5.
# Permutation 2: s1 marks row 1 done; s2's lock waits on it, rechecks the newest version (no longer 'new'), drops it and
# takes the next sorted row: 2 and 3.
# Permutation 3: s1 bumps row 1's prio; s2's lock waits, rechecks (still 'new') and locks the newest version; s2's write of
# row 1 then updates that version: owner 'b' over prio 12.
# Permutation 4: s1's claim holds rows 1 and 2 (now 'running'); s2's DELETE claim waits on row 1, drops both after s1
# commits and deletes 3 and 5.
setup
{
  CREATE TABLE jqh (id int PRIMARY KEY, state text NOT NULL, owner text, prio int NOT NULL);
  INSERT INTO jqh SELECT g, CASE WHEN g % 4 = 0 THEN 'done' ELSE 'new' END, NULL, (g * 7) % 5 FROM generate_series(1, 12) g;
  ANALYZE jqh;
}

teardown
{
  DROP TABLE jqh;
}

session s1
step s1b { BEGIN; }
step s1claim { UPDATE jqh SET state = 'running', owner = 'a' WHERE id IN (SELECT id FROM jqh WHERE state = 'new' ORDER BY id LIMIT 2 FOR UPDATE SKIP LOCKED) RETURNING id, state, owner; }
step s1done { UPDATE jqh SET state = 'done' WHERE id = 1; }
step s1bump { UPDATE jqh SET prio = prio + 10 WHERE id = 1; }
step s1c { COMMIT; }

session s2
step s2skip { UPDATE jqh SET state = 'running', owner = 'b' WHERE id IN (SELECT id FROM jqh WHERE state = 'new' ORDER BY id LIMIT 2 FOR UPDATE SKIP LOCKED) RETURNING id, state, owner; }
step s2wait { UPDATE jqh SET state = 'running', owner = 'b' WHERE id IN (SELECT id FROM jqh WHERE state = 'new' ORDER BY id LIMIT 2 FOR UPDATE) RETURNING id, state, owner, prio; }
step s2del { DELETE FROM jqh WHERE id IN (SELECT id FROM jqh WHERE state = 'new' ORDER BY id LIMIT 2 FOR UPDATE) RETURNING id; }
step s2read { SELECT id, state, owner, prio FROM jqh WHERE state <> 'new' OR owner IS NOT NULL ORDER BY id; }

permutation s1b s1claim s2skip s1c s2read
permutation s1b s1done s2wait s1c s2read
permutation s1b s1bump s2wait s1c s2read
permutation s1b s1claim s2del s1c s2read

# ROWMAP2 M0 (pgrc-plans/ROWMAP2.md): EvalPlanQual refetches a joined, non-target pgrcolumnar relation's row by its
# position (a reference row mark): UPDATE ... FROM and DELETE ... USING the pgrcolumnar table rates, whose target row a
# concurrent writer updates first, under READ COMMITTED; and a pgrcolumnar target joined with it. rates 1 sits in a
# sealed part (its position comes from the part's row map), rates 2 in the live segment. ROWMAP2_FORGET_TID=epq (scans
# of a reference-marked relation stamp no position) must turn this spec red. Expected output: the heap twin
# (scripts/pgrc-isolation.sh).

setup { CREATE TABLE acct (id int, bal int); INSERT INTO acct VALUES (1, 100), (2, 100); }
setup { CREATE TABLE rates (id int, r int) USING pgrcolumnar; }
setup { INSERT INTO rates VALUES (1, 3); }
setup { CALL pgrc_seal('rates'); }
setup { INSERT INTO rates VALUES (2, 5); }
setup { CREATE TABLE pacct (id int, bal int) USING pgrcolumnar; }
setup { INSERT INTO pacct VALUES (1, 100), (2, 100); }
setup { CALL pgrc_seal('pacct'); }

teardown { DROP TABLE acct; DROP TABLE rates; DROP TABLE pacct; }

session s1
step s1b    { BEGIN; }
step s1u1   { UPDATE acct SET bal = bal + 1 WHERE id = 1; }
step s1u2   { UPDATE acct SET bal = bal + 1 WHERE id = 2; }
step s1p1   { UPDATE pacct SET bal = bal + 1 WHERE id = 1; }
step s1c    { COMMIT; }

session s2
step s2b    { BEGIN; }
step s2uf1  { UPDATE acct a SET bal = a.bal * r.r FROM rates r WHERE a.id = r.id AND a.id = 1 RETURNING a.id, a.bal, r.r; }
step s2uf2  { UPDATE acct a SET bal = a.bal * r.r FROM rates r WHERE a.id = r.id AND a.id = 2 RETURNING a.id, a.bal, r.r; }
step s2du   { DELETE FROM acct a USING rates r WHERE a.id = r.id AND r.r = 3 RETURNING a.id, a.bal, r.r; }
step s2pf   { UPDATE pacct p SET bal = p.bal * r.r FROM rates r WHERE p.id = r.id AND p.id = 1 RETURNING p.id, p.bal, r.r; }
step s2c    { COMMIT; }

# each table read alone: with the Gravity/SQE slots at their defaults, sqe refuses a UNION ALL of the heap acct and the
# pgrcolumnar pacct over sealed parts (shape/seq-scan/strategy)
session r
step racct  { SELECT * FROM acct ORDER BY id; }
step rpacct { SELECT * FROM pacct ORDER BY id; }

# the joined row in a sealed part: refetched by its position after the target's update
permutation s1b s1u1 s2b s2uf1 s1c s2c racct rpacct
# the joined row in the live segment
permutation s1b s1u2 s2b s2uf2 s1c s2c racct rpacct
# DELETE ... USING
permutation s1b s1u1 s2b s2du s1c s2c racct rpacct
# a pgrcolumnar target joined with a pgrcolumnar relation
permutation s1b s1p1 s2b s2pf s1c s2c racct rpacct

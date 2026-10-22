# lockrows REFMERGE (gen lock-0009): Limit -> LockRows -> Merge Semi Join, the outer the locked relation's pkey scan, the
# inner the REFERENCE-marked relation's index scan under a filter (a.id IN (SELECT acct FROM rml WHERE amt = 99) .. FOR
# UPDATE OF a). Over a newer version of a (READ COMMITTED) PostgreSQL's EvalPlanQual reruns the merge over the test tuple
# and the rml row refetched at the row's junk ctid (the version the join read: a concurrent change to that rml row is not
# seen), so the row passes while its newest version still matches the merge clause; LockRows hands it to the Limit, which
# counts it. A deleted row is skipped and the Limit takes the next one; SKIP LOCKED skips a row s1 holds; NOWAIT raises
# 55P03. No extra row (main 22:51:30Z): a join qual that can raise (1 / (r.amt - 98 - 2 * a.ver)) is redone over the
# newest a version and the refetched rml row only, after the merge clause: rml 90001 (acct 100, amt 100) would raise
# division by zero over a.ver = 1 but PostgreSQL never reaches it (the semi join matched rml 99 first), and a re-keyed row
# fails the merge clause before the qual runs. D3373: an inner filter that can raise (1 / (amt + 1) > 0) over a ledger
# row s3 adds (acct 102, amt -1, after the four acct 102 rows in index order) raises division by zero only once the merge
# reads the ledger past acct 102 (outer row 103), after the recheck of row 100 over s1's newer version (the refetched rml
# row's filter redone). The merge reads the ledger by the statement's snapshot past the wait: rows s1 changes (amt -1 on
# acct 102) or adds and commits while s2 waits on row 100 are not seen. Expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE rma (id int PRIMARY KEY, bal numeric, ver int);
  INSERT INTO rma SELECT g, g * 10, 0 FROM generate_series(1, 2000) g;
  CREATE TABLE rml (id int PRIMARY KEY, acct int, amt numeric);
  INSERT INTO rml SELECT g, 1 + g % 2000, g % 100 FROM generate_series(1, 8000) g;
  INSERT INTO rml VALUES (90001, 100, 100);
  CREATE INDEX rml_acct ON rml (acct);
  ANALYZE rma, rml;
}
teardown { DROP TABLE rml, rma; }

session s1
setup		{ BEGIN; }
step s1v	{ UPDATE rma SET ver = ver + 1 WHERE id = 100; }
step s1i	{ UPDATE rma SET id = -100 WHERE id = 100; }
step s1d	{ DELETE FROM rma WHERE id = 100; }
step s1r	{ UPDATE rma SET id = -100, ver = 1 WHERE id = 100; }
step s1l	{ UPDATE rml SET amt = 0 WHERE acct = 100 AND amt = 99; }
step s1k	{ SELECT id FROM rma WHERE id = 100 FOR UPDATE; }
step s1m	{ UPDATE rml SET amt = -1 WHERE acct = 102; }
step s1n	{ INSERT INTO rml VALUES (90003, 102, -1); }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_nestloop = off; SET enable_seqscan = off; SET enable_bitmapscan = off; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT a.id FROM rma a WHERE a.id IN (SELECT acct FROM rml WHERE amt = 99) ORDER BY a.id LIMIT 3 FOR UPDATE OF a; }
step s2l	{ SELECT a.id, a.ver FROM rma a WHERE a.id IN (SELECT acct FROM rml WHERE amt = 99) ORDER BY a.id LIMIT 3 FOR UPDATE OF a; }
step s2s	{ SELECT a.id, a.ver FROM rma a WHERE a.id IN (SELECT acct FROM rml WHERE amt = 99) ORDER BY a.id LIMIT 3 FOR UPDATE OF a SKIP LOCKED; }
step s2n	{ SELECT a.id, a.ver FROM rma a WHERE a.id IN (SELECT acct FROM rml WHERE amt = 99) ORDER BY a.id LIMIT 3 FOR UPDATE OF a NOWAIT; }
step s2w	{ SELECT a.id, a.ver FROM rma a WHERE EXISTS (SELECT 1 FROM rml r WHERE r.acct = a.id AND r.amt > 98 AND 1 / (r.amt - 98 - 2 * a.ver) > 0) ORDER BY a.id LIMIT 3 FOR UPDATE OF a; }
step s2z	{ SELECT a.id, a.ver FROM rma a WHERE EXISTS (SELECT 1 FROM rml r WHERE r.acct = a.id AND r.amt > 98 AND 1 / (r.amt - 98 - a.ver) > 0) ORDER BY a.id LIMIT 3 FOR UPDATE OF a; }
step s2fx	{ EXPLAIN (COSTS OFF) SELECT a.id, a.ver FROM rma a WHERE a.id IN (SELECT acct FROM rml WHERE 1 / (amt + 1) > 0) AND a.id > 98 ORDER BY a.id LIMIT 4 FOR UPDATE OF a; }
step s2f	{ SELECT a.id, a.ver FROM rma a WHERE a.id IN (SELECT acct FROM rml WHERE 1 / (amt + 1) > 0) AND a.id > 98 ORDER BY a.id LIMIT 4 FOR UPDATE OF a; }
step s2g	{ SELECT a.id, a.ver FROM rma a WHERE a.id IN (SELECT acct FROM rml WHERE 1 / (amt + 1) > 0) AND a.id > 98 ORDER BY a.id LIMIT 5 FOR UPDATE OF a; }

session s3
step s3m	{ INSERT INTO rml VALUES (90002, 102, -1); }

# amt = 99 rows: rml.id = 99, 199, .. -> acct = 100, 200, ..
permutation s2x s1v s2l s1c
permutation s1i s2l s1c
permutation s1d s2l s1c
permutation s1l s1v s2l s1c
permutation s1k s2s s1c
permutation s1k s2n s1c
# no extra row: rml 99 rechecked over ver = 1 fails the qual (no raise; 90001 would raise); a re-key fails the merge
# clause first; over ver = 1 alone rml 99 raises division by zero in PostgreSQL's recheck too
permutation s1v s2w s1c
permutation s1r s2z s1c
permutation s1v s2z s1c
# D3373: the raising inner under the recheck of row 100: no error at LIMIT 4 (the merge stops at outer 102), division by
# zero at LIMIT 5 (outer 103 needs the ledger past acct 102)
permutation s3m s2fx s1v s2f s1c
permutation s3m s1v s2g s1c
# D3373: s1 sets acct 102's ledger rows to amt -1 (or adds one) and commits while s2 waits on row 100: the merge reads
# acct 102 after the wait by the statement's snapshot (the old amt 1 rows), so LIMIT 5 raises nothing
permutation s1v s1m s2g s1c
permutation s1v s1n s2g s1c

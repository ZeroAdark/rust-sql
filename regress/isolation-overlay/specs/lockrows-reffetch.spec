# lockrows REFFETCH: a LIMIT above a row lock over a Sort of an inner hash join whose other side is REFERENCE-marked
# (Limit -> LockRows -> Sort -> Hash Join -> (Seq Scan rfa, Hash -> Seq Scan rfo), FOR ... OF a). When s1 changed a
# locked row (READ COMMITTED), EvalPlanQual reruns the join over the newest version and the rfo row refetched at its junk
# ctid (EvalPlanQualFetchRowMark under SnapshotAny: the version the join read, even when s1 updated it too), and LockRows
# hands the result to the Limit: a row whose newest version still joins is counted where it stands in the sorted order
# (new values, not re-sorted); one whose owner moved (to another region, or to another owner of the same region: the
# refetched rfo row is the old owner's) or that was deleted is skipped and the next row taken; the rfo columns read above
# the join are the refetched version's; a join qual past the hash clause is rechecked; SKIP LOCKED skips a row s1 holds.
# D3379 REFCALM (s2z): a build-side qual that can raise (an integer division) but calls only builtin functions is
# redone over the refetched rfo row too, the version the join read, even when s1 moved that owner out of the region.
# Expected output is the C oracle's (PostgreSQL 18, pgdg).
setup
{
  CREATE TABLE rfa (id int PRIMARY KEY, owner text NOT NULL, bal int NOT NULL, w int NOT NULL);
  INSERT INTO rfa SELECT i, 'o' || (i % 12), (i * 37) % 101, i % 5 FROM generate_series(1, 600) i;
  CREATE TABLE rfo (owner text PRIMARY KEY, region text NOT NULL, cap int NOT NULL);
  INSERT INTO rfo SELECT 'o' || i, (ARRAY['eu', 'us', 'ap'])[1 + i % 3], 40 + i * 5 FROM generate_series(0, 11) i;
  ANALYZE rfa, rfo;
}
teardown { DROP TABLE rfa, rfo; }

session s1
setup		{ BEGIN; }
step s1b	{ UPDATE rfa SET bal = bal + 1000 WHERE id = 30; }
step s1w	{ UPDATE rfa SET w = 4 WHERE id = 30; }
step s1m	{ UPDATE rfa SET owner = 'o1' WHERE id = 30; }
step s1s	{ UPDATE rfa SET owner = 'o9' WHERE id = 30; }
step s1d	{ DELETE FROM rfa WHERE id = 30; }
step s1r	{ UPDATE rfo SET region = 'us', cap = 1 WHERE owner = 'o6'; }
step s1r9	{ UPDATE rfo SET region = 'us', cap = 1 WHERE owner = 'o9'; }
step s1p	{ UPDATE rfa SET bal = bal + 1 WHERE id = 237; }
step s1q	{ UPDATE rfa SET bal = 97 WHERE id = 237; }
step s1l	{ SELECT id FROM rfa WHERE id = 30 FOR UPDATE; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT a.id, a.bal FROM rfa a JOIN rfo o ON o.owner = a.owner WHERE o.region = 'eu' ORDER BY a.bal DESC, a.id LIMIT 3 FOR UPDATE OF a; }
step s2l	{ SELECT a.id, a.bal FROM rfa a JOIN rfo o ON o.owner = a.owner WHERE o.region = 'eu' ORDER BY a.bal DESC, a.id LIMIT 3 FOR UPDATE OF a; }
step s2w	{ SELECT a.id, a.bal, a.w FROM rfa a JOIN rfo o ON o.owner = a.owner WHERE o.region = 'eu' AND a.w < 4 ORDER BY a.bal DESC, a.id LIMIT 3 FOR NO KEY UPDATE OF a; }
step s2p	{ SELECT a.id, a.bal, o.region, o.cap FROM rfa a JOIN rfo o ON o.owner = a.owner AND o.cap > a.bal WHERE o.region = 'eu' ORDER BY a.bal DESC, a.id LIMIT 3 FOR SHARE OF a; }
step s2sk	{ SELECT a.id, a.bal FROM rfa a JOIN rfo o ON o.owner = a.owner WHERE o.region = 'eu' ORDER BY a.bal DESC, a.id LIMIT 2 FOR UPDATE OF a SKIP LOCKED; }
step s2z	{ SELECT a.id, a.bal, o.cap FROM rfa a JOIN rfo o ON o.owner = a.owner WHERE o.region = 'eu' AND 1000 / (o.cap - 30) > 5 ORDER BY a.bal DESC, a.id LIMIT 3 FOR UPDATE OF a; }

# region eu (o0 o3 o6 o9) by bal DESC: 30 (100, o6), 333 (100, o9), 60 (99, o0), 363 (99, o3), ...; with o.cap > a.bal
# (o9's cap 85): 237 (83), 297 (81), 357 (79), ...
permutation s2x s2l
permutation s1b s2l s1c
permutation s1w s2w s1c
permutation s1m s2l s1c
permutation s1s s2l s1c
permutation s1d s2l s1c
permutation s1r s1b s2l s1c
permutation s1r9 s1p s2p s1c
permutation s1q s2p s1c
permutation s1l s2sk s1c
permutation s1b s2z s1c
permutation s1r s1b s2z s1c

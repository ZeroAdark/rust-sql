# Tie-ordering conformance rules

Ratified rules for the ordering and selection of rows that compare equal on all
sort keys. Origin: the gm-merge-tree work (`notes/gm-merge-tree.md`, ratified by
Michael 2026-07-07, associativity proof there); promoted here because multiple
lanes need a citable conformance doc. New tie rules are appended here as they are
ratified.

## Baseline: what is and is not a compatibility surface

`heap_compare_slots` (a direct port of nodeGatherMerge.c) returns 0 on equal sort
keys; there is no tie-break in the comparator, in C or in the port. Within-tie-group
output order falls out of binary-heap structure over racy run contents and is
nondeterministic run to run — in C exactly as in pgrust.

Guaranteed surfaces (what gates may byte-compare):
- Key order across tie groups.
- The multiset of rows per equal-key group.
- LIMIT/OFFSET boundaries, rescan output, EXPLAIN shape, EXPLAIN ANALYZE row counts.

NOT a surface: the order of rows within an equal-key group. No query can rely on
it; SQL-level byte-compare gates must project payloads that are pure functions of
the sort key, or use order-insensitive checksums (see
`scripts/gm-tree-parity-e2e.sh` for the split-gate pattern).

## Rule 1: GatherMerge canonical order (origin-index tie-break)

For parallel merge (flat heap or merge tree) over runs R_0..R_{N-1}, one per
reader, the canonical output M is the N-way merge that, among minimal-key heads,
emits the head with the lowest origin reader index. Within an equal-key group, M
is origin-ascending concatenation, preserving per-origin run order. Any 2-way
merge tree whose internal nodes apply the same lower-origin-wins rule emits
exactly M regardless of tree shape or leaf placement (proof:
`notes/gm-merge-tree.md` §"Canonical rule and associativity proof").

This is a canonicalization, not a description of the literal heap: the heap agrees
with M on key order and multiset-per-key-group; its within-group order is
structural. Verify oracles must use the flat reference merge WITH the origin
tie-break, not the literal heap.

## Rule 2: bounded top-k canonical survivors ((key, rowref) total order)

Context: zone-ordered adaptive scans (`docs/design/pgrcolumnar-zone-adaptive.md`).
Under `ORDER BY x LIMIT k`, when rows tie at the k-th key, the ORDER of emitted
ties is covered above — but the SELECTION of which tie rows survive the bound is
payload-visible. The historical bounded heap used strict discard with
heap-shape-dependent eviction, so survivorship was an arrival-order accident.

Ratified rule: the pgrcolumnar topk-ref bounded heap orders by the total order
(sort key, then rowref ascending), where rowref = (row_group << 32) | rg-global-row
is monotone in physical position. Consequences:

- Survivors of a boundary tie group are the physically-earliest rows of that
  group — a deterministic function of the data, independent of scan visitation
  order (physical or zone-adaptive), by construction.
- The total order has no ties, so bound-domination checks are exact: a granule
  whose zone bound EQUALS the heap floor's key cannot be skipped (it may hold an
  equal key with a smaller rowref); skipping requires strict key domination.
- The rule applies in both the physical-order and adaptive topk-ref modes, so
  differential gates compare like against like.

C conformance: C's bounded sort under LIMIT returns an arbitrary legal choice of
boundary ties; regress expected outputs and the analytic suites' result hashes are
green under nondeterministic tie handling and are therefore tie-insensitive surfaces.
Pinning a canonical survivor rule selects one C-legal answer deterministically and
cannot un-match them.

Implementation (fix-100m-engagement, 2026-07): the lane's zone-adaptive sort
feed arms this rule as its default relaxed-mode tie handling
(`TieMode::Rowref` in `lanev2::sort_feed`; `Tuplesort::arm_topk_rowref` +
`puttupleslot_rowref`, rowref stamped in the minimal tuple's `mt_padding`).
Survivors are the canonical physically-earliest tie members, deterministic
and visitation-order-independent — the cut-selection demotion (which at
100M-scale key densities fired on every execution of a top-N over a low-cardinality
timestamp key, re-feeding the
whole scan) is retired for the relaxed default. Note C's own bounded heap
does NOT keep first-arrived tie members (replace-top evicts whichever equal
member sits at the root, a heap-shape accident), so the canonical set is
C-LEGAL but not byte-equal to a given lane-off run at a boundary tie — the
surface this rule's C-conformance paragraph ratifies. Gates therefore
byte-compare keys, per-key-group multisets and LIMIT/OFFSET boundaries, and
count-gate the (single) boundary tie group; `PGRUST_LANE_ADAPTIVE_TOPK=
tracked` keeps the demote-on-either-trigger byte-exact channel, and rule
3's "CutSelection demotes in every mode" clause is superseded by this
implementation for the relaxed default. A rowref contract break (a put
without a rowref, or a spilled bounded sort) still demotes to the physical
re-feed. OFFSET caveat: a tie group straddling the OFFSET boundary makes
the post-skip window within-group-order-dependent; such shapes remain
covered only by the boundary-count gate.

Rule-2 application — agg-group top-k selection (lane-v2 topkfin, hot-c1,
2026-07): on the admitted `GROUP BY … ORDER BY <int8 finalfn-none agg> LIMIT
k` shape (single sort column, bounded in-memory sort, relaxed mode only), the
lane selects the k surviving GROUPS on the raw compact-table states with a
bounded (key, insertion-row) total-order heap, then finalizes + forms tuples
for those k only (`nodeagg::topk_finalize_select`;
`lanev2::sort_feed_agg_topk_finalize`). The survivor set within the k-th
key's tie group is the first-arrived (group-birth-order) members —
deterministic and C-LEGAL under this rule's C-conformance paragraph, but not
byte-equal to a given lane-off run at a boundary tie (C's bounded heap keeps
a heap-shape-arbitrary subset of the same tie group). Gates count-gate the
boundary tie group exactly as for the scan-side rowref selection; the emitted
k rows are then fully ordered by the downstream bounded sort, so key order
and per-key-group multisets are exact. NULL order keys never take this path
(a NULL transvalue bails the pass before any side effect);
`PGRUST_LANE_ADAPTIVE_TOPK=tracked`/`0` and `PGRUST_LANE_V2_TOPKFIN=0` keep
the pre-existing byte-exact channels.

## Rule 3: relaxed retained-tie order for lane adaptive top-N (ratified 2026-07-12)

Context: the lane-v2 zone-ordered adaptive top-N sort feed
(`crates/backend/executor/execmain/src/lanev2.rs`, lane-v2-orderedscan /
lane-v2-relaxedtopk). The adaptive granule walk changes ARRIVAL order at the
bounded heap; with an unstable in-memory sort, the emit order of retained
equal-full-key rows is arrival-dependent.

Shape class: `ORDER BY <keys> LIMIT k` over a pgrcolumnar scan where some visible
output byte is NOT determined by the sort keys (payload-visible), the bounded
sort stays in memory, and full-key tie groups are retained inside the emitted
prefix. Examples: `SELECT phrase FROM t WHERE phrase <> '' ORDER BY ts LIMIT 10`
over a table sorted by a coarse timestamp (the armed walk skips 1204 of 1238
granules at 10M rows, with a boundary tie on `ts` at every execution), and
`SELECT * FROM t WHERE ... ORDER BY ts LIMIT 10`, where every column is
payload-visible.

Ratified rule (Michael, 2026-07-12: "different order is fine — relaxed
mode"): the default adaptive mode accepts the produced order WITHIN a
retained equal-full-key tie group instead of demoting to a physical re-feed.
The guaranteed surfaces are unchanged from the baseline section: key order
across tie groups, the multiset of rows per equal-key group, and LIMIT/OFFSET
boundaries. In particular the SELECTED SET stays exact: a CUT-SELECTION
trigger (which rows made the LIMIT cut is arrival-dependent — the tuplesort's
boundary-tie tracking, `TopkTieAmbiguity::CutSelection`) still demotes to the
byte-exact physical re-feed in every mode. `PGRUST_LANE_ADAPTIVE_TOPK=
tracked` (demote on either trigger) and `=0` remain the byte-exact channels.

Gate consequences (tie-group normalization — sort-normalize rows within
equal-sort-key groups on both sides before diffing):
- `scripts/lane-adaptivetopk-e2e.sh`: the relaxed default arm diffs
  tie-normalized against lane-off; off/kill/invisible/tracked arms stay raw
  byte-diffed; retained-tie SELECTION is additionally pinned by
  order-insensitive digest queries.
- `scripts/regress-order-compare.py`: `-- pgrust:stable-tie key=<cols>`
  implements per-tie-group normalization for annotated regress statements
  (exact key order across groups, multiset within groups).
- Result-hash comparisons: a query that projects its sort key is compared
  after tie-group normalization on that key; a query that does not project
  it (groups unrecoverable from the output) is compared by row count within
  the boundary tie group.

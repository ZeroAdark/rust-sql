//! vacuum_morsels — pure units for morselized VACUUM.
//!
//! inc-1 of docs/design/vacuum-morsels.md (RATIFIED 2026-07-13): the
//! VM-driven skip-map source (§3.1), the per-worker scan Locals with
//! order-free folds (§3.2), the round-quiesce protocol (§3.3, amended at
//! inc-1 — see [`QuiesceState`]), the dead-TID run merge, and the §5.2
//! coverage guard. NO executor wiring, NO buffer access: every unit is
//! driven by abstract oracles (a VM-status closure, simulated claim
//! schedules) so the property suites can exercise adversarial inputs.
//!
//! C ground truth: REL_18_3 vacuumlazy.c `heap_vac_scan_next_block` /
//! `find_next_unskippable_block` (:1571/:1676) for the skip rules
//! (eager region reset + remaining_fails admission included);
//! LVRelState counters (:313-357) for the fold classes (sums, XID mins,
//! block max, bool OR).

#![allow(non_snake_case)] // C-parity field names (NewRelfrozenXid class)

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use types_core::primitive::TransactionId;
use types_core::xact::{MultiXactIdPrecedes, TransactionIdIsNormal, TransactionIdPrecedes};
use types_core::InvalidBlockNumber;

/// C vacuumlazy.c:209 — runs of skippable pages shorter than this are
/// scanned anyway (kernel readahead beats the seek + rescan-risk math).
pub const SKIP_PAGES_THRESHOLD: u32 = 32;

/// C vacuumlazy.c:250 — eager-scan region width (blocks).
pub const EAGER_SCAN_REGION_SIZE: u32 = 4096;

/// Visibility-map bit layout parity (visibilitymap.h). The wiring increment
/// asserts these against the `visibilitymap` crate's constants; the pure
/// units keep the crate dependency-thin.
pub const VM_ALL_VISIBLE: u8 = 0x01;
pub const VM_ALL_FROZEN: u8 = 0x02;

// ---------------------------------------------------------------------------
// §3.1 VacuumBlockSource — VM skip logic as granule admission
// ---------------------------------------------------------------------------

/// Inputs to the skip-map build; mirrors the LVRelState fields the C skip
/// logic reads, including eager-scan region state (vacuumlazy.c:366-410).
#[derive(Clone, Copy, Debug)]
pub struct SkipMapParams {
    pub rel_pages: u32,
    /// First block this round considers (0 for round 0, the previous
    /// round's resume block after a quiesce).
    pub resume_block: u32,
    pub aggressive: bool,
    /// false under DISABLE_PAGE_SKIPPING — nothing is skipped.
    pub skipwithvm: bool,
    /// `InvalidBlockNumber` disables eager admission (aggressive / small /
    /// success-capped / rate-0). Otherwise the first block of the next
    /// eager region (C `next_eager_scan_region_start`).
    pub next_eager_scan_region_start: u32,
    pub eager_scan_remaining_fails: u32,
    pub eager_scan_max_fails_per_region: u32,
}

/// One admitted (unskippable-or-short-run) block: the granule unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanBlock {
    pub block: u32,
    /// C's VAC_BLK_ALL_VISIBLE_ACCORDING_TO_VM side channel: the VM
    /// ALL_VISIBLE bit observed at skip-decision time.
    pub all_visible_according_to_vm: bool,
    /// C's VAC_BLK_WAS_EAGER_SCANNED: all-visible not-frozen, admitted
    /// because the region's failure budget was still open.
    pub was_eager_scanned: bool,
}

/// The per-round granule space: every block the round will scan, ascending,
/// plus the skip decisions' `skipsallvis` verdict (computed here because it
/// is a property of the decisions, all of which are made here — doc §3.1).
#[derive(Debug)]
pub struct VacuumBlockSource {
    entries: Vec<ScanBlock>,
    skipsallvis: bool,
    rel_pages: u32,
    /// Every SKIPPED run (>= threshold), by its position in granule space:
    /// `(pos, has_avnotaf)` where `pos` = the index of the first granule
    /// AFTER the run. inc-2 addition: a quiesced round consumes a skip
    /// decision iff `resume_granule >= pos` (the resume block is the granule
    /// at `resume_granule`, so every skipped run below it is jumped over
    /// permanently); runs above the resume point are RE-DECIDED by the next
    /// round's map. Committing the whole-map verdict on a tripped round
    /// would over-suppress relfrozenxid advancement (safe but breaks the
    /// on/off parity oracle).
    skip_runs: Vec<(u64, bool)>,
    /// Walk leftover after region resets; admission does not debit fails.
    pub eager_scan_remaining_fails: u32,
    pub next_eager_scan_region_start: u32,
    /// Initial `SkipMapParams::next_eager_scan_region_start` (Invalid when
    /// eager is disabled). Hard-boundary math uses this, not the walked
    /// cursor.
    eager_region_start: u32,
}

impl VacuumBlockSource {
    /// Build the round's skip map by C's rules (find_next_unskippable_block
    /// :1676 + the threshold arm :1624), restated run-wise: classify each
    /// block skippable/unskippable, then a maximal skippable run is skipped
    /// iff it is >= SKIP_PAGES_THRESHOLD long; a skipped run containing an
    /// all-visible-but-not-all-frozen page commits `skipsallvis` (:1628 —
    /// relfrozenxid must not advance past unfrozen skipped pages).
    ///
    /// Classification per block (C rule order; `vm` is the status oracle —
    /// `visibilitymap_get_status` at wiring, synthetic bits in tests):
    ///   - the LAST block is never skippable (:1728, nonempty_pages needs it)
    ///   - `!skipwithvm` makes every block unskippable (:1732)
    ///   - not ALL_VISIBLE => unskippable
    ///   - ALL_FROZEN => skippable (:1739)
    ///   - aggressive => unskippable (:1746 — av-not-af must be scanned)
    ///   - eager remaining_fails > 0 => unskippable + was_eager_scanned (:1754)
    ///   - else skippable, and the run remembers it held an av-not-af page
    ///     (:1764 `*skipsallvis = true`)
    ///
    /// Blocks of a SHORT skippable run are emitted as granules with their
    /// av flag set (C case 2, :1633-1644); their run's av-not-af marker is
    /// NOT committed (nothing was skipped).
    pub fn build(p: &SkipMapParams, mut vm: impl FnMut(u32) -> u8) -> Self {
        let mut entries = Vec::new();
        let mut skipsallvis = false;
        let mut skip_runs: Vec<(u64, bool)> = Vec::new();
        let mut remaining_fails = p.eager_scan_remaining_fails;
        let mut next_region = p.next_eager_scan_region_start;

        // Pending maximal skippable run: [run_start, b) with its av-not-af
        // marker. Flushed when an unskippable block (or the end) is reached.
        let mut run: Option<(u32, bool)> = None;

        let flush_run = |run: &mut Option<(u32, bool)>,
                         end: u32,
                         entries: &mut Vec<ScanBlock>,
                         sav: &mut bool,
                         skip_runs: &mut Vec<(u64, bool)>| {
            if let Some((start, has_avnotaf)) = run.take() {
                if end - start >= SKIP_PAGES_THRESHOLD {
                    // Actually skipped: commit the verdict, emit nothing.
                    // Position = the granule index of the block that ends
                    // the run (about to be pushed by the caller).
                    skip_runs.push((entries.len() as u64, has_avnotaf));
                    if has_avnotaf {
                        *sav = true;
                    }
                } else {
                    // Short run: scanned anyway, all-visible per the VM.
                    for b in start..end {
                        entries.push(ScanBlock {
                            block: b,
                            all_visible_according_to_vm: true,
                            was_eager_scanned: false,
                        });
                    }
                }
            }
        };

        for b in p.resume_block..p.rel_pages {
            let status = vm(b);
            let all_visible = status & VM_ALL_VISIBLE != 0;
            let all_frozen = status & VM_ALL_FROZEN != 0;
            let last = b == p.rel_pages - 1;

            if b >= next_region {
                remaining_fails = p.eager_scan_max_fails_per_region;
                next_region = next_region.wrapping_add(EAGER_SCAN_REGION_SIZE);
            }

            let mut was_eager_scanned = false;
            let skippable = if last || !p.skipwithvm || !all_visible {
                false
            } else if all_frozen {
                true
            } else if p.aggressive {
                false
            } else if remaining_fails > 0 {
                was_eager_scanned = true;
                false
            } else {
                true
            };

            if skippable {
                let avnotaf = !all_frozen;
                match &mut run {
                    Some((_, has)) => *has |= avnotaf,
                    None => run = Some((b, avnotaf)),
                }
            } else {
                flush_run(&mut run, b, &mut entries, &mut skipsallvis, &mut skip_runs);
                entries.push(ScanBlock {
                    block: b,
                    all_visible_according_to_vm: all_visible,
                    was_eager_scanned,
                });
            }
        }
        // A trailing skippable run is impossible: the last block is never
        // skippable, so any pending run was flushed by it. (rel_pages == 0
        // or resume >= rel_pages yields an empty map.)
        debug_assert!(run.is_none() || p.resume_block >= p.rel_pages);

        VacuumBlockSource {
            entries,
            skipsallvis,
            rel_pages: p.rel_pages,
            skip_runs,
            eager_scan_remaining_fails: remaining_fails,
            next_eager_scan_region_start: next_region,
            eager_region_start: p.next_eager_scan_region_start,
        }
    }

    pub fn entries(&self) -> &[ScanBlock] {
        &self.entries
    }

    /// granule index -> block (claims are granule ranges; the scan body
    /// resolves each granule to its heap block through this).
    pub fn block_of(&self, granule: u64) -> ScanBlock {
        self.entries[granule as usize]
    }

    pub fn skipsallvis(&self) -> bool {
        self.skipsallvis
    }

    /// The `skipsallvis` verdict restricted to skip decisions CONSUMED by a
    /// round that resumed at `resume_granule` (inc-2): a skipped run at
    /// position `pos` (index of the first granule after it) is jumped over
    /// permanently iff `resume_granule >= pos` — the next round's map starts
    /// at the resume granule's BLOCK, past the run. Runs above the resume
    /// point are re-decided from fresh VM state and must not commit here.
    /// `skipsallvis_before(total_granules()) == skipsallvis()`.
    pub fn skipsallvis_before(&self, resume_granule: u64) -> bool {
        self.skip_runs.iter().any(|&(pos, avnotaf)| avnotaf && resume_granule >= pos)
    }

    pub fn rel_pages(&self) -> u32 {
        self.rel_pages
    }
}

impl runtime::MorselSource for VacuumBlockSource {
    fn total_granules(&self) -> u64 {
        self.entries.len() as u64
    }

    fn next_boundary_after(&self, start: u64) -> u64 {
        let total = self.entries.len() as u64;
        if start >= total || self.eager_region_start == InvalidBlockNumber {
            return total;
        }
        let start_block = self.entries[start as usize].block;
        let next_region = if start_block < self.eager_region_start {
            self.eager_region_start
        } else {
            let off = start_block - self.eager_region_start;
            self.eager_region_start
                + ((off / EAGER_SCAN_REGION_SIZE) + 1) * EAGER_SCAN_REGION_SIZE
        };
        for i in (start + 1)..total {
            if self.entries[i as usize].block >= next_region {
                return i;
            }
        }
        total
    }

    fn startup_c0(&self) -> u64 {
        16 // m1-heap-source's reasoning transfers (doc §3.1)
    }
}

/// Shared eager-scan caps for morsel workers (doc §3.4), the order-free
/// form of vacuumlazy.c's cursor bookkeeping (:1715-1719 region reset,
/// :1768-1771 admission, :1425-1463 accounting).
///
/// C admits an all-visible-not-frozen block while the region's failure
/// budget is open and stops admitting once `eager_scan_remaining_successes`
/// hits 0 after a freeze; its read stream decides block N+1 before block N
/// is accounted, so on a cached scan exactly S+1 eager blocks are admitted
/// (S = the success cap). Workers scan claims concurrently, so the serial
/// "decide after the previous prune" form is not reproducible; instead the
/// success cap is a CREDIT pool of S+1 consumed at admission (refunded when
/// the page turns out not to need freezing, which C charges to the region's
/// failure budget instead). Any claim schedule then admits exactly S+1
/// eager blocks when every one freezes — C's cached-scan count — and the
/// per-region failure budget is shared by region exactly as C resets it at
/// each region start.
pub struct EagerScanShared {
    /// S+1 admission credits (see the type doc); 0 = the success cap closed.
    credits: AtomicU32,
    /// Set when the success cap closes: C :1456-1458 zeroes the fail budget,
    /// the region cursor and the per-region maximum together.
    disabled: AtomicBool,
    max_fails_per_region: AtomicU32,
    first_region_start: u32,
    region_fails: Vec<AtomicU32>,
    orig_success_limit: u32,
}

/// One admission verdict for an eager candidate block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EagerAdmit {
    /// Skip the block (cap closed, region failure budget spent).
    Deny,
    /// Scan the block as eagerly scanned.
    Admit,
    /// Scan it, and this admission closed the success cap: emit C's
    /// "disabling eager scanning after freezing %u eagerly scanned blocks"
    /// report once (:1444-1449) with the carried original limit.
    AdmitAndClose(u32),
}

fn eager_region_id(first_start: u32, block: u32) -> usize {
    if first_start == InvalidBlockNumber || block < first_start {
        0
    } else {
        1 + ((block - first_start) / EAGER_SCAN_REGION_SIZE) as usize
    }
}

impl EagerScanShared {
    pub fn new(
        remaining_successes: u32,
        first_region_fails: u32,
        max_fails_per_region: u32,
        first_region_start: u32,
        rel_pages: u32,
    ) -> Self {
        let nregions = if first_region_start == InvalidBlockNumber {
            1
        } else {
            1 + (rel_pages.saturating_sub(first_region_start) / EAGER_SCAN_REGION_SIZE) as usize + 1
        };
        let region_fails = (0..nregions)
            .map(|i| {
                AtomicU32::new(if i == 0 {
                    first_region_fails
                } else {
                    max_fails_per_region
                })
            })
            .collect();
        // heap_vacuum_eager_scan_setup disables eager scanning outright when
        // the cap is 0 (:570), so an enabled state always carries S >= 1;
        // a disabled state (max_fails == 0) never admits regardless.
        let disabled = max_fails_per_region == 0;
        EagerScanShared {
            credits: AtomicU32::new(if disabled { 0 } else { remaining_successes.saturating_add(1) }),
            disabled: AtomicBool::new(disabled),
            max_fails_per_region: AtomicU32::new(max_fails_per_region),
            first_region_start,
            region_fails,
            orig_success_limit: remaining_successes,
        }
    }

    /// C's `eager_scan_remaining_successes` view of the credit pool: the
    /// look-ahead credit is not a success.
    pub fn remaining_successes(&self) -> u32 {
        if self.disabled.load(Ordering::Relaxed) {
            0
        } else {
            self.credits.load(Ordering::Relaxed).saturating_sub(1)
        }
    }

    pub fn max_fails_per_region(&self) -> u32 {
        self.max_fails_per_region.load(Ordering::Relaxed)
    }

    pub fn disabled(&self) -> bool {
        self.disabled.load(Ordering::Relaxed)
    }

    pub fn fails_at(&self, block: u32) -> u32 {
        self.region_fails
            .get(eager_region_id(self.first_region_start, block))
            .map(|a| a.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Would `block` be admitted right now (no credit consumed)? The
    /// worker's run walk (SKIP_PAGES_THRESHOLD, C :1636) peeks with this.
    pub fn would_admit(&self, block: u32) -> bool {
        !self.disabled.load(Ordering::Relaxed)
            && self.fails_at(block) > 0
            && self.credits.load(Ordering::Relaxed) > 0
    }

    /// Admit `block` (an all-visible-not-frozen candidate the skip map
    /// carried as eager) at scan time: C :1768-1771 gated on the region's
    /// open failure budget, the success cap as the credit pool.
    pub fn try_admit(&self, block: u32) -> EagerAdmit {
        if self.disabled.load(Ordering::Relaxed) || self.fails_at(block) == 0 {
            return EagerAdmit::Deny;
        }
        let Ok(prev) = self
            .credits
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| (n > 0).then_some(n - 1))
        else {
            return EagerAdmit::Deny;
        };
        if prev - 1 == 0 {
            self.close();
            EagerAdmit::AdmitAndClose(self.orig_success_limit)
        } else {
            EagerAdmit::Admit
        }
    }

    /// C :1456-1458 — permanently disable eager scanning.
    fn close(&self) {
        self.disabled.store(true, Ordering::Relaxed);
        self.max_fails_per_region.store(0, Ordering::Relaxed);
        for a in &self.region_fails {
            a.store(0, Ordering::Relaxed);
        }
    }

    /// Post-prune accounting of an admitted block (C :1425-1463): a freeze
    /// keeps its credit; a page that did not need freezing refunds it and
    /// spends one of the region's failures instead (:1461-1462).
    pub fn account(&self, block: u32, vm_page_frozen: bool) {
        if vm_page_frozen || self.disabled.load(Ordering::Relaxed) {
            return;
        }
        let _ = self.credits.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| Some(n + 1));
        if let Some(a) = self
            .region_fails
            .get(eager_region_id(self.first_region_start, block))
        {
            let _ = a.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n > 0).then_some(n - 1)
            });
        }
    }

    /// Leader fold-back after a morsel round: `(successes, fails, max_fails,
    /// next_region_start)` at `at_block`, in C's field terms — the serial
    /// arm resuming from here re-applies its own read-stream look-ahead, so
    /// the look-ahead credit is not handed over twice.
    pub fn snapshot(&self, at_block: u32) -> (u32, u32, u32, u32) {
        let max = self.max_fails_per_region.load(Ordering::Relaxed);
        if max == 0 || self.disabled.load(Ordering::Relaxed) {
            return (0, 0, 0, InvalidBlockNumber);
        }
        let next = if self.first_region_start == InvalidBlockNumber {
            InvalidBlockNumber
        } else if at_block < self.first_region_start {
            self.first_region_start
        } else {
            self.first_region_start
                + ((at_block - self.first_region_start) / EAGER_SCAN_REGION_SIZE + 1)
                    * EAGER_SCAN_REGION_SIZE
        };
        (self.remaining_successes(), self.fails_at(at_block), max, next)
    }
}

// ---------------------------------------------------------------------------
// §3.2 Scan Locals — order-free counter folds + dead-TID runs
// ---------------------------------------------------------------------------

/// The LVRelState counter block as a fold value. Every member is one of the
/// three order-insensitive-exact fold classes (doc §1.1): SUM, MIN under
/// TransactionIdPrecedes, MAX. `fold` is commutative and associative over
/// the domain vacuum produces (all folded XIDs are normal and within 2^31
/// of each other — the OldestXmin seed upper-bounds them), so the seal
/// result is byte-identical under any claim schedule or fold order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanCounters {
    // SUM folds (C vacuumlazy.c LVRelState :313-357).
    pub scanned_pages: u64,
    pub lpdead_item_pages: u64,
    pub missed_dead_pages: u64,
    pub new_frozen_tuple_pages: u64,
    pub vm_new_visible_pages: u64,
    pub vm_new_visible_frozen_pages: u64,
    pub vm_new_frozen_pages: u64,
    pub tuples_deleted: u64,
    pub tuples_frozen: u64,
    pub lpdead_items: u64,
    pub live_tuples: u64,
    pub recently_dead_tuples: u64,
    pub missed_dead_tuples: u64,
    // MAX fold: blkno+1 of the last block seen with tuples (:341 — feeds
    // the truncation gate).
    pub nonempty_pages: u32,
    // MIN folds under TransactionIdPrecedes, seeded from the generation's
    // cutoffs exactly as C seeds from OldestXmin/OldestMxact (:193 in the
    // port; pruneheap threads them as in/out running minimums).
    pub NewRelfrozenXid: TransactionId,
    pub NewRelminMxid: TransactionId, // MultiXactId == TransactionId alias
}

impl ScanCounters {
    pub fn seed(oldest_xmin: TransactionId, oldest_mxact: TransactionId) -> Self {
        ScanCounters {
            scanned_pages: 0,
            lpdead_item_pages: 0,
            missed_dead_pages: 0,
            new_frozen_tuple_pages: 0,
            vm_new_visible_pages: 0,
            vm_new_visible_frozen_pages: 0,
            vm_new_frozen_pages: 0,
            tuples_deleted: 0,
            tuples_frozen: 0,
            lpdead_items: 0,
            live_tuples: 0,
            recently_dead_tuples: 0,
            missed_dead_tuples: 0,
            nonempty_pages: 0,
            NewRelfrozenXid: oldest_xmin,
            NewRelminMxid: oldest_mxact,
        }
    }

    /// Fold `other` into `self`. Commutative + associative (see type doc).
    pub fn fold(&mut self, other: &ScanCounters) {
        self.scanned_pages += other.scanned_pages;
        self.lpdead_item_pages += other.lpdead_item_pages;
        self.missed_dead_pages += other.missed_dead_pages;
        self.new_frozen_tuple_pages += other.new_frozen_tuple_pages;
        self.vm_new_visible_pages += other.vm_new_visible_pages;
        self.vm_new_visible_frozen_pages += other.vm_new_visible_frozen_pages;
        self.vm_new_frozen_pages += other.vm_new_frozen_pages;
        self.tuples_deleted += other.tuples_deleted;
        self.tuples_frozen += other.tuples_frozen;
        self.lpdead_items += other.lpdead_items;
        self.live_tuples += other.live_tuples;
        self.recently_dead_tuples += other.recently_dead_tuples;
        self.missed_dead_tuples += other.missed_dead_tuples;
        self.nonempty_pages = self.nonempty_pages.max(other.nonempty_pages);
        debug_assert!(
            TransactionIdIsNormal(other.NewRelfrozenXid),
            "vacuum folds only normal XIDs (seeded from cutoffs)"
        );
        // MultiXactIds have their own validity floor: FirstMultiXactId == 1
        // is a legitimate OldestMxact on a fresh cluster (a "not normal"
        // value by XID rules — the inc-2 CI cluster battery caught the stricter
        // assert panicking on the very first engaged fold). Compare with
        // MultiXactIdPrecedes, C's own comparator for this tracker.
        debug_assert!(other.NewRelminMxid != 0, "vacuum folds only valid MultiXactIds");
        if TransactionIdPrecedes(other.NewRelfrozenXid, self.NewRelfrozenXid) {
            self.NewRelfrozenXid = other.NewRelfrozenXid;
        }
        if MultiXactIdPrecedes(other.NewRelminMxid, self.NewRelminMxid) {
            self.NewRelminMxid = other.NewRelminMxid;
        }
    }
}

/// One block's dead items, recorded in scan order. Blocks are visited by
/// exactly one worker (granule ownership), so runs never overlap across
/// Locals and the merge is a disjoint k-way merge (§3.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeadRun {
    pub block: u32,
    pub offsets: Vec<u16>, // OffsetNumber, ascending (C: qsort before add)
}

/// A claim's outcome in a Local. Under the claim-boundary quiesce (see
/// [`QuiesceState`]) a claim is either fully scanned (`scanned == true`)
/// or a post-trip no-op (`scanned == false`, nothing touched).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClaimRecord {
    pub start: u64,
    pub end: u64,
    pub scanned: bool,
}

/// Per-worker scan Local (doc §3.2): plain owned Send data, no Mcx borrows.
#[derive(Debug)]
pub struct VacScanLocal {
    pub worker: usize,
    pub counters: ScanCounters,
    /// Dead-TID runs, ascending by block within the Local (claim order ×
    /// block order within a claim).
    pub runs: Vec<DeadRun>,
    pub claims: Vec<ClaimRecord>,
    /// §5.3 deferred-cleanup ledger: blocks the conditional cleanup lock +
    /// noprune ladder could not satisfy (aggressive-must-freeze); processed
    /// serially by the leader between SCAN and INDEX.
    pub deferred_cleanup: Vec<u32>,
    /// Blocks this Local scanned as eagerly scanned (C's
    /// `eager_scanned_pages`, :1336-1337) — a SUM fold.
    pub eager_scanned_pages: u64,
    /// Granules whose eager admission was denied at scan time and whose
    /// skip run reached SKIP_PAGES_THRESHOLD (C :1636-1641): each is an
    /// all-visible-not-frozen page left unscanned, so the leader folds
    /// `skippedallvis` for every one below the round's resume point.
    pub eager_skipped_granules: Vec<u64>,
}

impl VacScanLocal {
    pub fn new(worker: usize, oldest_xmin: TransactionId, oldest_mxact: TransactionId) -> Self {
        VacScanLocal {
            worker,
            counters: ScanCounters::seed(oldest_xmin, oldest_mxact),
            runs: Vec::new(),
            claims: Vec::new(),
            deferred_cleanup: Vec::new(),
            eager_scanned_pages: 0,
            eager_skipped_granules: Vec::new(),
        }
    }

    pub fn record_dead(&mut self, block: u32, offsets: Vec<u16>) {
        debug_assert!(offsets.windows(2).all(|w| w[0] < w[1]), "offsets ascending");
        debug_assert!(
            self.runs.last().is_none_or(|r| r.block < block),
            "runs ascend within a Local"
        );
        self.runs.push(DeadRun { block, offsets });
    }
}

// ---------------------------------------------------------------------------
// §3.3 Round quiesce (AMENDED at inc-1: trip at CLAIM boundaries)
// ---------------------------------------------------------------------------

/// Shared round-quiesce state.
///
/// inc-1 AMENDMENT to the ratified §3.3 (recorded in the doc + gate
/// record): the trip is observed at CLAIM boundaries, not block
/// boundaries. Per-block tripping does NOT yield a contiguous-prefix
/// scanned set: a straggler mid-way through an OLDER claim plus a fully
/// completed NEWER claim leaves scanned islands above the min-unscanned
/// point, and islands force either double-scan (counter parity break) or
/// island-carrying resume state. Claim-boundary tripping restores the
/// prefix invariant structurally: claims are carved contiguously off one
/// cursor, every claim STARTED before the trip runs to completion, every
/// claim observed AFTER the trip is a no-op — so the scanned set is
/// exactly the claims carved before the trip = a contiguous granule
/// prefix. Overshoot bound restated: at most K in-flight CLAIMS' worth of
/// dead TIDs past max_bytes (each claim duration-bounded by the sizer's
/// t_max; C's own guarantee is one page's worth past the limit).
/// Cost-delay and failsafe checks remain per-block (C cadence).
pub struct QuiesceState {
    max_bytes: u64,
    bytes: AtomicU64,
    tripped: AtomicBool,
}

impl QuiesceState {
    pub fn new(max_bytes: u64) -> Self {
        QuiesceState { max_bytes, bytes: AtomicU64::new(0), tripped: AtomicBool::new(false) }
    }

    /// Account `n` estimated dead-TID bytes; trips the round when the
    /// shared total crosses `max_bytes`. Called per block (accounting
    /// cadence is C's); the returned/observed trip takes effect at the
    /// worker's NEXT claim.
    pub fn add_bytes(&self, n: u64) {
        let total = self.bytes.fetch_add(n, Ordering::Relaxed) + n;
        if total > self.max_bytes {
            self.tripped.store(true, Ordering::Release);
        }
    }

    /// Claim-entry check: a claim that observes the trip is a no-op.
    pub fn tripped(&self) -> bool {
        self.tripped.load(Ordering::Acquire)
    }

    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
}

/// SCAN-finalize resume computation: the smallest granule of any no-op'd
/// claim (everything below it was scanned; everything at/above it was not
/// — the claim-boundary prefix invariant). `total` when nothing tripped.
pub fn resume_granule(locals: &[VacScanLocal], total: u64) -> u64 {
    locals
        .iter()
        .flat_map(|l| l.claims.iter())
        .filter(|c| !c.scanned)
        .map(|c| c.start)
        .min()
        .unwrap_or(total)
}

/// §5.2 coverage guard, structural half: the scanned claims across all
/// Locals must tile `[0, resume)` exactly — no hole, no overlap, nothing
/// scanned at/above `resume`. Any violation means a Local was lost or the
/// protocol was breached; the caller then suppresses relfrozenxid
/// advancement (always-safe, C's skippedallvis behavior) — it never folds
/// a wrong minimum.
#[derive(Debug, PartialEq, Eq)]
pub enum CoverageError {
    HoleAt(u64),
    OverlapAt(u64),
    ScannedBeyondResume { start: u64, resume: u64 },
}

pub fn verify_prefix_coverage(
    locals: &[VacScanLocal],
    resume: u64,
) -> Result<(), CoverageError> {
    let mut scanned: Vec<(u64, u64)> = locals
        .iter()
        .flat_map(|l| l.claims.iter())
        .filter(|c| c.scanned)
        .map(|c| (c.start, c.end))
        .collect();
    scanned.sort_unstable();
    let mut next = 0u64;
    for (s, e) in scanned {
        if s >= resume {
            return Err(CoverageError::ScannedBeyondResume { start: s, resume });
        }
        match s.cmp(&next) {
            std::cmp::Ordering::Greater => return Err(CoverageError::HoleAt(next)),
            std::cmp::Ordering::Less => return Err(CoverageError::OverlapAt(s)),
            std::cmp::Ordering::Equal => next = e,
        }
    }
    if next != resume {
        return Err(CoverageError::HoleAt(next));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// §3.2 run merge — the round dead-items authority
// ---------------------------------------------------------------------------

/// Merge per-worker dead-TID runs into the round's block-ascending dead
/// list (the authority phase II snapshots and phase III walks). Runs are
/// disjoint across Locals and ascending within one, so this is a k-way
/// merge of sorted sequences with no key collisions — O(total dead TIDs),
/// the doc §3.2 "relocation, not addition" cost.
pub fn merge_dead_runs(locals: &[VacScanLocal]) -> Vec<DeadRun> {
    let mut cursors: Vec<std::iter::Peekable<std::slice::Iter<'_, DeadRun>>> =
        locals.iter().map(|l| l.runs.iter().peekable()).collect();
    let total: usize = locals.iter().map(|l| l.runs.len()).sum();
    let mut out = Vec::with_capacity(total);
    loop {
        let mut best: Option<(usize, u32)> = None;
        for (i, c) in cursors.iter_mut().enumerate() {
            if let Some(r) = c.peek() {
                if best.is_none_or(|(_, b)| r.block < b) {
                    best = Some((i, r.block));
                }
            }
        }
        let Some((i, blk)) = best else { break };
        let run = cursors[i].next().expect("peeked").clone();
        debug_assert!(
            out.last().is_none_or(|p: &DeadRun| p.block < blk),
            "blocks are disjoint across Locals and ascending within one"
        );
        out.push(run);
    }
    out
}

#[cfg(test)]
mod tests;

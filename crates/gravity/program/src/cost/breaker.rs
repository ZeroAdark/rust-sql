//! The one breaker-cost function (stats item 4; gap-optoverfit §3.6, mechanism C): a share-budgeted breaker's bytes from the
//! record the lowering laid out and the rows the plan estimated. The grant ask (`grant_need`) sums it; a family that sizes a
//! table, a top-k budget, a partition fan-out or a reserve from a breaker asks it instead of a width guess of its own (DR-1..3,
//! DR-12, DR-18, DC-04, STEP-21 adopt it in their lanes). Split out of the grant ask with the same bytes for every program.
use gravity_abi::breaker::{opened_bits, BreakerKind, Budget, Spill, BLOCK_BYTES, BLOCK_FULL_CHAIN, HASHBUILD_ENTRY, HASHBUILD_HDR, t1_default_bytes};

use super::{FALLBACK_RECORD, HELD_TABLES, PARTITION_BYTES};
use crate::Pipeline;

/// What one share breaker fills: its record, the rows it is sized by, the largest table a share opens, and (D365) the records
/// a partitioned record table's workers hold until the owner phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BreakerCost {
    pub kind: BreakerKind,
    /// One record's bytes: a hash build's entry and stored row (`store_row`), a sort's run row, else the owner layout's
    /// size, else `FALLBACK_RECORD` (a breaker with no layout to read), each with the planner's row width as its floor
    /// (DR-1 / DR-4).
    pub record: u64,
    /// The rows it is sized by: a hash build's `build_estimate` (PostgreSQL's rows at its Hash node), else `input_rows`
    /// (D4047: a drain's named rows, the planner's rows at its input where fewer than its driving scan's, else D3205's).
    pub rows: u64,
    /// The largest table one share opens: a partitioned breaker's `PARTITION_BYTES << bits` (a hash build's at least its
    /// table), a hash build's power-of-two table and row store, else `rows x record` to a power of two (a row top's rows
    /// at most its bound, `Pipeline::top_rows`).
    pub table: u64,
    /// D365: a partitioned record table's held records (every input row's at worst), bounded by `HELD_TABLES` of its bits'
    /// table, plus a quarter for the claims' imbalance and the blocks' slack; 0 for every other breaker.
    pub held: u64,
    /// DR-6: a partitioned record table's lowered partition bits (its share opens a phase-1 T1 and a block pool at the bits
    /// `opened_bits` gives); `None` for a hash build and every unpartitioned breaker, which open no T1.
    pub parts: Option<u8>,
}

/// D3054: the planner's bytes of a grouping-set input (rows x (record + width)) a record spool may hold: the spool
/// (decompose gsets `Spool::resident`, lowerv4 runs.gsets_spool) has no spill, so a larger input keeps the Sort run, which
/// spills. A memory bound for a store that does not spill, not a tuned speed: no measurement chose it.
pub const SPOOL_RESIDENT_BYTES: u64 = 1 << 30;

/// C10: the largest program constant a breaker may keep in memory as its proof (`spill::Proof::Constant`): PostgreSQL's
/// least work_mem (64 kB), so PostgreSQL keeps that much in memory at every work_mem it accepts; a larger constant is
/// debt (`spill::NoSpill::ConstantPastCap`). A PostgreSQL limit, not a tuned size.
pub const CONSTANT_PROOF_BYTES: u32 = 64 << 10;

/// D3054: the records one claim of a grouping-set spool's reader reads (exec nest.spool.chunks): a claim's open is a few
/// loads, so a run of a heap block's rows or so; not a tuned speed (no measurement chose it).
pub const SPOOL_CLAIM_RECORDS: u64 = 1024;

/// DR-6 (B): one partition's pool bytes, what `T1::at_sized` lays a chain out for (at least four blocks, aiming at
/// `BLOCK_FULL_CHAIN` full `BLOCK_BYTES` blocks): format facts, not the 512 KiB slice of groups.
pub const POOL_PART_BYTES: u64 = BLOCK_FULL_CHAIN as u64 * BLOCK_BYTES as u64;

impl BreakerCost {
    /// DR-6 (B): the bytes one share opens past its T1 at `width`: a partitioned record table's pool for the bits the kernels
    /// open (`opened_bits`: the kernels' floor of 6 at width 8 or more, fold_entries.rs `agg_open`; an arm run's
    /// `ARM_RUN_PBITS` is the same 6), else `table` (a hash build's table and row store, a sort's or a materialize's rows).
    pub const fn share_at(&self, width: u16) -> u64 {
        match self.parts {
            Some(bits) => POOL_PART_BYTES << opened_bits(bits, width, false),
            None => self.table,
        }
    }

    /// DR-6 (1): the T1 one share opens: a partitioned record table's phase-1 table (the kernels reserve
    /// `T1_DEFAULT_BYTES`; table/entries.rs `agg_open_grown` grows into it from the estimate's size); 0 for a hash build,
    /// a sort, a materialize or a top-k, which open none.
    pub fn t1(&self) -> u64 {
        match self.parts {
            Some(_) => t1_default_bytes() as u64,
            None => 0,
        }
    }
}

/// The cost of `p`'s breaker when it is share-budgeted; `None` for a sink, a program-constant breaker or one without a budget.
pub fn of(p: &Pipeline) -> Option<BreakerCost> {
    of_at(p, false, false)
}

/// `of` under `pgrust.gravity_exact_size`. exactsize B1c: `exact_build` sizes a hash build's table by its records alone
/// (`build_records_bytes`), not the directory and row store `build_bytes` estimates; the directory is laid from the counted
/// rows at the build's end and drawn there (exactsize B1a). D3205: `end_rows` sizes any other breaker by `sized_rows`.
/// `false, false` is `of`, byte for byte.
pub fn of_at(p: &Pipeline, exact_build: bool, end_rows: bool) -> Option<BreakerCost> {
    let b = p.breaker()?;
    if !b.budget.is_share() {
        return None;
    }
    let hash = b.kind == BreakerKind::HashBuild;
    let record = record(p);
    let rows = if hash { p.build_estimate() } else { input_rows(p, end_rows) };
    let build = |rows: u64, row: u64| if exact_build { build_records_bytes(rows, row) } else { build_bytes(rows, row) };
    let table = match b.spill {
        Spill::Partitioned { bits } if hash => (PARTITION_BYTES << bits.min(20)).max(build(rows, store_row(p))),
        Spill::Partitioned { bits } => PARTITION_BYTES << bits.min(20),
        _ if hash => build(rows, store_row(p)), // V4R-481: the rows its Hash receives
        Spill::Runs | Spill::None => rows_bytes(if p.top_rows != 0 { rows.min(p.top_rows) } else { rows }, record),
    };
    // D365: a partitioned record table's workers hold what they evict until the owner phase, every input row's at worst
    // (60 M rows over ~2 M groups once asked 384 MiB and scattered 2.2 GB), so one leader share covers them (a worker's is
    // it / W) with a quarter for the claims' imbalance and the blocks' slack; past it they scatter to files. Bounded by
    // `HELD_TABLES` of the bits' table (the planner's groups x record): a few-group scan evicts next to nothing
    let (held, parts) = match b.spill {
        Spill::Partitioned { bits } if !hash => (rows.saturating_mul(record).min(table.saturating_mul(HELD_TABLES)).saturating_mul(5) / 4, Some(bits)),
        _ => (0, None),
    };
    Some(BreakerCost { kind: b.kind, record, rows, table, held, parts })
}

/// D4047: the rows a breaker that is not a hash build is sized by. First the rows its lowering named (D3355:
/// `Pipeline::build_rows`, a fused RIGHT SEMI / ANTI drain's probe, whose own rows never reach its end); else, where its
/// pipeline names both, the planner's rows at its input (`Pipeline::end_rows`) when they are fewer than its driving scan's
/// (`predicted_rows`): past a narrowing join a resident or partitioned aggregate holds, a Sort, a row top, a Materialize or
/// a TopK opens what reaches it (D4043: a GROUP BY past three joins receives a third of its scan's rows), never more (a fanning
/// join's rows never grow what it reserves); else `sized_rows` (D3205). A statement that takes the program binds its own
/// `end_rows` (`bind_estimates`).
pub const fn input_rows(p: &Pipeline, end_rows: bool) -> u64 {
    if p.build_rows != 0 {
        p.build_rows
    } else if p.predicted_rows != 0 && p.end_rows != 0 && p.end_rows < p.predicted_rows {
        p.end_rows
    } else {
        sized_rows(p, end_rows)
    }
}

/// D3205: the rows a breaker that is not a hash build is sized by: its pipeline's `predicted_rows`, else (`end_rows`, under
/// `pgrust.gravity_exact_size` = final | all) the planner's rows at the pipeline's end (`Pipeline::end_rows`). A serial
/// pipeline (a breaker's consumer, an index or TID source, a CTE scan) carries no `predicted_rows`, so its Sort,
/// Materialize or hash aggregate was sized from 0 rows while its plan node had an estimate (memcensus g-rows0: 239 such
/// breakers, a CTE's Materialize touching 1 GiB from a 0 share).
pub const fn sized_rows(p: &Pipeline, end_rows: bool) -> u64 {
    if end_rows && p.predicted_rows == 0 {
        p.end_rows
    } else {
        p.predicted_rows
    }
}

/// `p`'s record bytes as the breaker cost reads them (`BreakerCost::record`): the lowering's (a hash build's entry and
/// stored row, a sort's run row (RUNS-26: `store_row_bytes`, lowerv4 face/runs.rs `Shape::row_bytes`), else the owner
/// layout's size, else `FALLBACK_RECORD`) with the planner's width of the rows it receives (`Pipeline::row_width`) as the
/// floor (DR-1).
pub fn record(p: &Pipeline) -> u64 {
    match p.breaker() {
        Some(b) if b.kind == BreakerKind::HashBuild => HASHBUILD_ENTRY as u64 + store_row(p),
        Some(b) if b.kind == BreakerKind::Sort && p.store_row_bytes != 0 => row_bytes(p.store_row_bytes as u64, p.row_width),
        _ => row_bytes(p.owner.as_ref().map_or(FALLBACK_RECORD, |o| o.layout.size() as u64), p.row_width),
    }
}

/// A hash build's stored row bytes: its row store's (`store_row_bytes`, the lowering's words) with the planner's width as
/// the floor (DR-4: a by-reference column's word stands for the bytes PostgreSQL estimated); 0 for the legacy inline entry,
/// which stores no row.
pub fn store_row(p: &Pipeline) -> u64 {
    match p.store_row_bytes {
        0 => 0,
        s => row_bytes(s as u64, p.row_width),
    }
}

/// A row's bytes from the lowering's fixed layout with the planner's width (`plan_width`, from `stawidth`) as its floor: a
/// by-reference column's layout holds a reference, the row it stands for is at least the width PostgreSQL estimated.
pub const fn row_bytes(layout: u64, plan_width: u32) -> u64 {
    let w = plan_width as u64;
    let b = if layout > w { layout } else { w };
    if b == 0 {
        1
    } else {
        b
    }
}

/// `rows` records of `record` bytes held in one table: their bytes, to a power of two (at least 1).
pub const fn rows_bytes(rows: u64, record: u64) -> u64 {
    let b = rows.saturating_mul(record);
    if b == 0 {
        1
    } else {
        b.next_power_of_two()
    }
}

/// A hash build of `rows` hashed in place (`HashBuild::hash_in_place`; partitioned D133, a drain's D146): the power-of-two
/// table at 7/8 load behind its header, and the stored rows of `row` bytes.
pub const fn build_bytes(rows: u64, row: u64) -> u64 {
    (rows.saturating_mul(8) / 7 + 1).next_power_of_two().saturating_mul(HASHBUILD_ENTRY as u64).saturating_add(rows.saturating_mul(row)).saturating_add(HASHBUILD_HDR as u64 + 4096)
}

/// exactsize B1c: a hash build's records alone, `rows` stored rows of `row` bytes behind the header (`build_bytes` without
/// its estimate-sized directory): what the build holds before its end counts the rows and lays the directory.
pub const fn build_records_bytes(rows: u64, row: u64) -> u64 {
    rows.saturating_mul(row).saturating_add(HASHBUILD_HDR as u64 + 4096)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{End, OwnerDesc};
    use gravity_abi::breaker::{Breaker, KeyKind, RecordLayout, SlotKind};

    fn share(kind: BreakerKind, spill: Spill) -> Breaker {
        Breaker { spill, budget: Budget::Share, ..Breaker::sized(kind, 0) }
    }

    #[test]
    fn a_hash_build_is_sized_by_its_entry_store_and_build_rows() {
        let p = Pipeline { build_rows: 1_000, store_row_bytes: 24, ..Pipeline::eligible(0, End::Breaker(share(BreakerKind::HashBuild, Spill::None)), 60_000) };
        let c = of(&p).expect("a share breaker");
        assert_eq!((c.record, c.rows, c.held), (HASHBUILD_ENTRY as u64 + 24, 1_000, 0));
        assert_eq!(c.table, build_bytes(1_000, 24));
        let part = Pipeline { ends_in: End::Breaker(share(BreakerKind::HashBuild, Spill::Partitioned { bits: 4 })), ..p };
        assert_eq!(of(&part).unwrap().table, (PARTITION_BYTES << 4).max(build_bytes(1_000, 24)));
    }

    /// exactsize B1c: `exact_build` sizes a hash build's table by its records alone, the directory left to the count; every
    /// other breaker, and every breaker without it, costs as `of` does.
    #[test]
    fn exact_build_asks_a_hash_build_for_its_records_alone() {
        let p = Pipeline { build_rows: 15_000_000, store_row_bytes: 48, ..Pipeline::eligible(0, End::Breaker(share(BreakerKind::HashBuild, Spill::None)), 60_000) };
        assert_eq!(of_at(&p, false, false), of(&p));
        let (was, now) = (of(&p).unwrap(), of_at(&p, true, false).unwrap());
        assert_eq!(now.table, 15_000_000 * 48 + HASHBUILD_HDR as u64 + 4096);
        assert_eq!(was.table - now.table, (1u64 << 25) * HASHBUILD_ENTRY as u64, "the estimate's 2^25-entry directory leaves the ask");
        assert_eq!(BreakerCost { table: was.table, ..now }, was, "only the table differs");
        let part = Pipeline { ends_in: End::Breaker(share(BreakerKind::HashBuild, Spill::Partitioned { bits: 4 })), ..p };
        assert_eq!(of_at(&part, true, false).unwrap().table, (PARTITION_BYTES << 4).max(build_records_bytes(15_000_000, 48)));
        let layout = RecordLayout::new(&[KeyKind::I32], &[SlotKind::Count]);
        for (kind, spill) in [(BreakerKind::HashAgg, Spill::Partitioned { bits: 3 }), (BreakerKind::Distinct, Spill::None), (BreakerKind::Sort, Spill::Runs)] {
            let q = Pipeline { owner: Some(OwnerDesc::generic(layout, 0, 0)), ..Pipeline::eligible(0, End::Breaker(share(kind, spill)), 1_000_000) };
            assert_eq!(of_at(&q, true, false), of(&q), "{kind:?}");
        }
        let both = [p, part];
        assert_eq!(super::super::grant_need_of_at(&both, 2, 8, false, false), super::super::grant_need_of(&both, 2, 8));
        assert!(super::super::grant_need_of_at(&both, 2, 8, true, false) < super::super::grant_need_of(&both, 2, 8));
    }

    /// D3205: a serial pipeline (no `predicted_rows`) ends in a Sort, a Materialize or a hash aggregate whose plan node has
    /// an estimate (`end_rows`): `end_rows` sizes it by that estimate, not 0; a pipeline with its own rows, a hash build and
    /// every breaker without it cost as `of` does.
    #[test]
    fn end_rows_sizes_a_serial_pipelines_breaker_by_its_plan_rows() {
        let layout = RecordLayout::new(&[KeyKind::I32], &[SlotKind::Count]);
        let serial = |kind, spill| Pipeline { end_rows: 153_989, row_width: 64, owner: Some(OwnerDesc::generic(layout, 0, 0)), ..Pipeline::serial(-1, End::Breaker(share(kind, spill)), crate::Reason::NoDrivingSite) };
        for (kind, spill) in [(BreakerKind::Sort, Spill::Runs), (BreakerKind::Materialize, Spill::None), (BreakerKind::HashAgg, Spill::Partitioned { bits: 3 }), (BreakerKind::Distinct, Spill::None)] {
            let q = serial(kind, spill);
            let (was, now) = (of(&q).unwrap(), of_at(&q, false, true).unwrap());
            assert_eq!((was.rows, now.rows), (0, 153_989), "{kind:?}");
            assert_eq!(of_at(&q, true, false), of(&q), "{kind:?}: build alone leaves it");
            match spill {
                Spill::Partitioned { .. } => assert!(now.held > was.held, "{kind:?}: its held records count the rows"),
                _ => assert_eq!((was.table, now.table), (rows_bytes(0, was.record), rows_bytes(153_989, now.record)), "{kind:?}"),
            }
            let own = Pipeline { predicted_rows: 1_000, ..q };
            assert_eq!(of_at(&own, false, true), of(&own), "{kind:?}: a pipeline's own rows stand");
        }
        let build = Pipeline { build_rows: 1_000, store_row_bytes: 24, end_rows: 9_999_999, ..Pipeline::serial(-1, End::Breaker(share(BreakerKind::HashBuild, Spill::None)), crate::Reason::NoDrivingSite) };
        assert_eq!(of_at(&build, false, true), of(&build), "a hash build is sized by its Hash rows");
        let sorts = [serial(BreakerKind::Sort, Spill::Runs)];
        assert_eq!(super::super::grant_need_of_at(&sorts, 1, 8, false, false), super::super::grant_need_of(&sorts, 1, 8));
        assert!(super::super::grant_need_of_at(&sorts, 1, 8, false, true) > super::super::grant_need_of(&sorts, 1, 8));
    }

    /// D3355: a breaker whose rows the lowering named (`build_rows`: a fused RIGHT SEMI / ANTI drain's probe, whose own
    /// rows never reach its end) is sized by them, not by its driving scan's rows: a semi-join probe of a 60 M-row scan
    /// sized its GROUP BY table by all 60 M rows, an anti-join probe of a 600 M-row scan a 13.3 GB build.
    #[test]
    fn a_breaker_with_named_rows_is_sized_by_them_not_by_its_scan() {
        let layout = RecordLayout::new(&[KeyKind::I32], &[SlotKind::Count]);
        let agg = Pipeline { owner: Some(OwnerDesc::generic(layout, 0, 0)), ..Pipeline::eligible(5, End::Breaker(share(BreakerKind::HashAgg, Spill::None)), 59_986_052) };
        let scan = of(&agg).unwrap();
        assert_eq!(scan.rows, 59_986_052, "no rows named: the driving scan's");
        let named = of(&Pipeline { build_rows: 1, ..agg }).unwrap();
        assert_eq!((named.rows, named.table), (1, rows_bytes(1, named.record)), "named: the drain's rows");
        let build = Pipeline { store_row_bytes: 56, ..Pipeline::eligible(4, End::Breaker(share(BreakerKind::HashBuild, Spill::None)), 200_012_634) };
        assert_eq!(of(&build).unwrap().rows, 200_012_634);
        assert_eq!(of(&Pipeline { build_rows: 1, ..build }).unwrap().rows, 1, "a hash build's named rows (`build_estimate`)");
    }

    #[test]
    fn a_record_table_reads_its_owner_layout_and_holds_its_evictions() {
        let layout = RecordLayout::new(&[KeyKind::I32], &[SlotKind::Count]);
        let rec = layout.size() as u64;
        let p = Pipeline { owner: Some(OwnerDesc::generic(layout, 0, 0)), ..Pipeline::eligible(0, End::Breaker(share(BreakerKind::HashAgg, Spill::Partitioned { bits: 3 })), 10_000_000) };
        let c = of(&p).unwrap();
        assert_eq!((c.record, c.rows, c.table), (rec, 10_000_000, PARTITION_BYTES << 3));
        assert_eq!(c.held, (10_000_000 * rec).min((PARTITION_BYTES << 3) * HELD_TABLES) * 5 / 4);
    }

    #[test]
    fn a_breaker_without_a_layout_takes_the_fallback_record() {
        let p = Pipeline::eligible(0, End::Breaker(share(BreakerKind::HashAgg, Spill::Runs)), 1_000);
        let c = of(&p).unwrap();
        assert_eq!((c.record, c.table, c.held), (FALLBACK_RECORD, (1_000 * FALLBACK_RECORD).next_power_of_two(), 0));
        assert_eq!(of(&Pipeline::eligible(0, End::Sink, 1_000)), None, "a sink has no breaker");
    }

    #[test]
    fn the_planner_width_floors_every_breaker_record() {
        // DR-1 / DR-4: a wide row (a by-reference column the layout holds a word for) asks its width, a narrow one its layout
        let wide = |p: Pipeline| Pipeline { row_width: 300, ..p };
        let build = Pipeline { build_rows: 1_000, store_row_bytes: 24, ..Pipeline::eligible(0, End::Breaker(share(BreakerKind::HashBuild, Spill::None)), 60_000) };
        assert_eq!((of(&wide(build)).unwrap().record, of(&wide(build)).unwrap().table), (HASHBUILD_ENTRY as u64 + 300, build_bytes(1_000, 300)));
        assert_eq!(of(&Pipeline { row_width: 8, ..build }).unwrap(), of(&build).unwrap(), "a width under the store's words asks the words");
        let inline = Pipeline { store_row_bytes: 0, ..build };
        assert_eq!(of(&wide(inline)).unwrap(), of(&inline).unwrap(), "the legacy inline entry stores no row to floor");
        let layout = RecordLayout::new(&[KeyKind::I32], &[SlotKind::Count]);
        let rec = layout.size() as u64;
        let agg = Pipeline { owner: Some(OwnerDesc::generic(layout, 0, 0)), ..Pipeline::eligible(0, End::Breaker(share(BreakerKind::HashAgg, Spill::Partitioned { bits: 3 })), 10_000_000) };
        assert_eq!((record(&wide(agg)), record(&Pipeline { row_width: 4, ..agg })), (300, rec));
        let runs = Pipeline::eligible(0, End::Breaker(share(BreakerKind::Sort, Spill::Runs)), 1_000);
        assert_eq!((of(&wide(runs)).unwrap().table, of(&Pipeline { row_width: 16, ..runs }).unwrap().record), ((1_000 * 300u64).next_power_of_two(), FALLBACK_RECORD));
    }

    #[test]
    fn runs26_a_sort_reads_its_lowered_run_row() {
        // the run row the lowering published, the planner's width its floor; none published: the fallback record's
        let sort = |store, width| Pipeline { store_row_bytes: store, row_width: width, ..Pipeline::eligible(0, End::Breaker(share(BreakerKind::Sort, Spill::Runs)), 1_000) };
        assert_eq!(record(&sort(24, 8)), 24);
        assert_eq!(record(&sort(24, 40)), 40);
        assert_eq!(record(&sort(0, 8)), row_bytes(FALLBACK_RECORD, 8));
        assert_eq!(of(&sort(24, 8)).unwrap().table, rows_bytes(1_000, 24));
        // a row top's bound (`top_rows`) sizes the table when it is under the rows; never past them
        assert_eq!(of(&sort(24, 8).keeping_rows(400)).unwrap().table, rows_bytes(400, 24));
        assert_eq!(of(&sort(24, 8).keeping_rows(4_000)).unwrap().table, rows_bytes(1_000, 24));
        assert_eq!(of(&sort(24, 8).keeping_rows(400)).unwrap().held, 0);
        // a hash build's store is read as before; another breaker kind does not read the field
        let mat = Pipeline { store_row_bytes: 24, ..Pipeline::eligible(0, End::Breaker(share(BreakerKind::Materialize, Spill::Runs)), 1_000) };
        assert_eq!(record(&mat), FALLBACK_RECORD);
    }

    #[test]
    fn dr6_a_partitioned_record_table_asks_its_pool_at_the_bits_it_opens() {
        // DR-6 (B): 6 full 32 KiB blocks a partition at the opened bits (the kernels' 6 at W >= 8), its T1 apart
        let layout = RecordLayout::new(&[KeyKind::I32], &[SlotKind::Count]);
        let agg = |bits| Pipeline { owner: Some(OwnerDesc::generic(layout, 0, 0)), ..Pipeline::eligible(0, End::Breaker(share(BreakerKind::HashAgg, Spill::Partitioned { bits })), 1_000) };
        let c = of(&agg(3)).unwrap();
        assert_eq!((POOL_PART_BYTES, c.parts, c.table), (192 << 10, Some(3), PARTITION_BYTES << 3), "the table stays the groups' bytes (held's bound)");
        assert_eq!((c.share_at(1), c.share_at(4), c.share_at(8), c.share_at(16)), (3 << 19, 3 << 19, 12 << 20, 12 << 20));
        assert_eq!((of(&agg(11)).unwrap().share_at(8), of(&agg(11)).unwrap().table), (384 << 20, 1 << 30), "bits 11: 384 MiB, was 1 GiB");
        assert_eq!(c.t1(), t1_default_bytes() as u64);
        let distinct = Pipeline { ends_in: End::Breaker(share(BreakerKind::Distinct, Spill::Partitioned { bits: 7 })), ..agg(3) };
        assert_eq!((of(&distinct).unwrap().share_at(8), of(&distinct).unwrap().t1()), (POOL_PART_BYTES << 7, t1_default_bytes() as u64));
        // the ask: every worker's share and T1, the owner's share
        assert_eq!(super::super::grant_need_of(&[agg(3)], 1, 8), Some((12 << 20) * 9 + 8 * (t1_default_bytes() as u64)));
        assert_eq!(super::super::grant_need_of(&[agg(3)], 1, 1), Some((3 << 19) * 2 + t1_default_bytes() as u64));
    }

    #[test]
    fn dr6_a_hash_build_a_sort_and_a_materialize_open_no_t1() {
        // DR-6 (1): only a partitioned record table opens a phase-1 T1; the others ask their table alone
        let build = Pipeline { build_rows: 1_000, store_row_bytes: 24, ..Pipeline::eligible(0, End::Breaker(share(BreakerKind::HashBuild, Spill::Partitioned { bits: 4 })), 60_000) };
        let runs = Pipeline::eligible(0, End::Breaker(share(BreakerKind::Sort, Spill::Runs)), 1_000);
        let mat = Pipeline::eligible(0, End::Breaker(share(BreakerKind::Materialize, Spill::Runs)), 1_000);
        let unpart = Pipeline::eligible(0, End::Breaker(share(BreakerKind::HashAgg, Spill::Runs)), 1_000);
        for p in [build, runs, mat, unpart] {
            let c = of(&p).unwrap();
            assert_eq!((c.parts, c.t1(), c.share_at(8), c.share_at(1)), (None, 0, c.table, c.table), "{:?}", c.kind);
            assert_eq!(super::super::grant_need_of(&[p], 1, 8), Some(c.table * 9), "{:?}", c.kind);
        }
    }

    #[test]
    fn the_planner_width_is_the_row_floor() {
        assert_eq!((row_bytes(16, 40), row_bytes(64, 40), row_bytes(0, 0)), (40, 64, 1));
        assert_eq!((rows_bytes(0, 64), rows_bytes(3, 24)), (1, 128));
    }
}

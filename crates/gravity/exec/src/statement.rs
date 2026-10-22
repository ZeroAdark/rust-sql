//! M1's runtime protocols (plan §4.3, the end-to-end slice): the `ProtoKind`s of milestone 1, over hosts v4 m1's builders;
//! the site admissions (ADMIT / BIND) are m1_admit.rs's, the keyed and join protocols m1_keyed.rs's. Created and closed
//! by M1 (owner M1-HOSTS).
use std::collections::HashMap;

use gravity_abi::breaker::ADAPT_FACTOR;
use gravity_driver::Exec;
use gravity_hosts::ops::scan::pgrc::granule_source;
use gravity_hosts::ops::agg::hosts as m1;
use gravity_program::cost::runtime::{dense_span_pays, OBSERVED_CAP};
use gravity_program::protos::{Fact, Proto, ProtoKind};
use gravity_program::{LaneId, SiteKind};
use types_error::PgResult;
use types_storage::lock::NoLock;

use crate::ops::{Live, Prepare, Protocol};
use crate::glue;


pub use crate::ops::scan::sites::{ADMIT, BIND};
use crate::ops::sort::topn::proto::{BuildSide, RtFilter, TwoPass, TwoPassFloor};
use crate::ops::scan::pgrc::verdicts::Verdicts;
/// Width first: a later protocol may read `cx.widths`.
pub const PROTOCOLS: &[&dyn Protocol] = &[&Width, &OrderedSink, &Verdicts, &TwoPass, &TwoPassFloor, &BuildSide, &RtFilter, &Facts, &Observe];

/// `width.work` / `width.write`: the pipeline's width from this execution's bind facts (`knobs::width::pipeline_width`,
/// M1-DRIVER; `arg` bit 63 = leader-only).
struct Width;

impl Protocol for Width {
    fn kind(&self) -> ProtoKind {
        ProtoKind::Width
    }
    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        let w = crate::inputs::knobs::width::pipeline_width(cx, p)?;
        match cx.widths.get_mut(p.pipeline as usize) {
            Some(slot) => *slot = w,
            None => return Err(glue::internal("gravity: width.work on a pipeline the program does not have")),
        }
        Ok(None)
    }
}

/// `sink.ordered`: the ordered sink is the IR's own (an ordered pipeline runs on one lane, M1-DRIVER); nothing at prepare.
struct OrderedSink;

impl Protocol for OrderedSink {
    fn kind(&self) -> ProtoKind {
        ProtoKind::OrderedSink
    }
    fn prepare(&self, _p: &Proto, _cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        Ok(None)
    }
}

/// `facts` (H13b table.sized and the dense domain, M1-TABLES' declare_facts): bit f of `arg` writes Fact f into the next
/// word from `words.base`, in Fact order, from this execution's snapshot. DenseLo / DenseHi: lane `slot` of site `site`,
/// the parts' exact extrema, or a heap site's column statistics (`stats_range`) when their span pays over the rows that
/// reach the table (`dense_span_pays`: observed, else the planner's) ((1, 0) none: the table stays hashed).
/// TableSize, a group bound: the dense span when `observe` remembered rows enough for it (KEYED-22: cost
/// `dense_span_pays` over this execution's width, so a table.small opens dense though its last run saw fewer groups than
/// the span), else the groups `observe` remembered for the table, else the dense span capped by the surviving rows, else
/// those rows (0 none, a heap site).
/// Another fact is not built at M1: its prepare fails.
struct Facts;

impl Protocol for Facts {
    fn kind(&self) -> ProtoKind {
        ProtoKind::Facts
    }
    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        let (base, n, k) = (p.words.base as usize, p.words.n as usize, p.pipeline as usize);
        if p.arg.count_ones() as usize != n || base + n > cx.words.len().min(cx.nulls.len()) {
            return Err(glue::internal("gravity: facts whose words do not match its bits"));
        }
        // D3024 (b): a build drain's driving site is nominal (its probe's: the drain reads its build's records, no row
        // of that relation), so its table's facts are a site-less reader's, as before the drain ran wide
        let site = usize::try_from(p.site).ok().or_else(|| cx.program.pipelines.get(k).filter(|q| !matches!(q.claims, gravity_program::ClaimKind::BuildRecords { .. })).and_then(|q| usize::try_from(q.driving_site).ok()));
        let lane = (p.site >= 0).then_some(p.slot as usize);
        let observed = observed(cx.program.key.0, k);
        let width = cx.widths.get(k).copied().unwrap_or(1);
        let (rows, dense) = match site.and_then(|s| Some((s, cx.admitted.registry.granule_recipe(s)?))) {
            Some((s, r)) => {
                let rel = table::table_open(cx.mcx, r.rel, NoLock)?;
                // NEW-72: the zone predicates as this execution binds them (`site_zone`; none when a bound one has no value)
                let zone = crate::ops::scan::pgrc::verdicts::site_zone(cx.program, s, &cx.words[..], &cx.nulls[..]).unwrap_or_default();
                let facts = granule_source::resolve(&rel, Some(cx.snap)).and_then(|l| m1::site_facts(&rel, &l, r, &zone, lane));
                rel.close(NoLock)?;
                facts?
            }
            None if p.arg >> Fact::DenseLo as u8 & 1 != 0 => {
                // SRC-38: the statistics' span pays only over the rows that reach the table (KEYED-22's rule): the rows
                // `observe` remembered at a later execution, else this statement's planner rows after the scan's filters
                // (`admit::bound`; 0 for a serial walk: no domain until a run is observed). A filtered GROUP BY that folds
                // few rows into a wide column's span stays hashed.
                let reach = observed.map_or_else(|| crate::admit::bound(cx.admitted, k).map_or(0, |q| q.predicted_rows), |(_, folded, _)| folded);
                let range = site.zip(lane).map_or(Ok(None), |(s, l)| stats_range(cx, s, l))?.filter(|&(lo, hi)| dense_span_pays(hi.abs_diff(lo).saturating_add(1), reach, width));
                if range.is_some() {
                    fires::fire("dense.heap_stats"); // shapecheck: a heap key's dense domain from its statistics (SRC-38)
                }
                (0, range)
            }
            None => (0, None),
        };
        // SRC-38 slice 2: no exact or statistics domain: the key range the table's owners folded at this statement's last
        // execution (`OwnerDesc::observe_key`) under the same `memo_key` (the statement words: a literal-free program's
        // filters differ by their words, so `k >= 4950` never opens at `k >= 0`'s range or the reverse; and the driving
        // heap's storage: a table truncated, rewritten or grown since is not read at its old keys), when its span pays
        // over the rows that run folded (KEYED-22's rule) and stays within `ADAPT_FACTOR` of the groups it held
        // (stats_range's sparse cap). A range, never a bound: a key past it takes the dense table's overflow, and this
        // run's owners observe the range again.
        let now = match (dense, observed) {
            (None, Some((_, _, Some(_)))) if p.arg >> Fact::DenseLo as u8 & 1 != 0 => Some(memo_key(cx, k)?),
            _ => None,
        };
        let dense = match (dense, observed) {
            (None, Some((groups, folded, Some((lo, hi, memo))))) if now == Some(memo) => {
                let span = hi.abs_diff(lo).saturating_add(1);
                let range = (dense_span_pays(span, folded, width) && (hi.abs_diff(lo) as f64) < ADAPT_FACTOR as f64 * groups.max(1) as f64).then_some((lo, hi));
                if range.is_some() {
                    fires::fire("dense.observed"); // shapecheck: a key's dense domain from its last execution's range (SRC-38)
                }
                range
            }
            (d, _) => d,
        };
        let span = dense.map(|(lo, hi)| hi.abs_diff(lo).saturating_add(1));
        for (i, f) in Fact::ALL.into_iter().filter(|&f| p.arg >> f as u8 & 1 != 0).enumerate() {
            cx.words[base + i] = match f {
                Fact::DenseLo => dense.map_or(1, |d| d.0) as u64,
                Fact::DenseHi => dense.map_or(0, |d| d.1) as u64,
                Fact::TableSize => match (observed, span) {
                    (Some((_, folded, _)), Some(span)) if dense_span_pays(span, folded, width) => {
                        fires::fire("table.sized"); // shapecheck fire signal (KEYED-22): a later execution sized at the dense span its last run's rows cover
                        span
                    }
                    (Some((groups, ..)), _) => groups,
                    (None, span) => span.map_or(rows, |n| n.min(rows)),
                },
                f => return Err(glue::internal(&format!("gravity: Fact::{f:?} is not built at M1"))), // alloc: Cold
            };
            cx.nulls[base + i] = 0;
        }
        let sited = site.is_some_and(|s| cx.admitted.registry.granule_recipe(s).is_some());
        Ok(LateSize::of(p, cx, sited).map(|l| Box::new(l) as Box<dyn Live>)) // alloc: Exec
    }
}

/// exactsize B4 (under `pgrust.gravity_exact_size` final or all): a keyed table fed by a finished slot (no driving site:
/// the consumer walks a breaker's records, `Pipeline::serial(-1, ..)`, so its planner rows are 0 and its TableSize opened
/// the full phase-1 default) takes its TableSize, late, from the producer's counted records (`Owned::counts` of the
/// slot's leader region after the producer ran): an exact bound on the rows the table folds. Laid as a late word at the
/// consumer's start (`Live::late_words`), never above the prepared word when that named one (an observed run's groups).
/// Only a TableSize-only Facts (no dense domain: the span is a domain, not a group bound) of a pipeline whose site reads no
/// relation (`sited` false: its prepared rows are 0) and that reads one slot from its producer (its `consumes_from` life,
/// else the nearest earlier producer), claiming its source (no spill partitions). A sizing fact, never a bound: a table
/// past it grows as today. A producer whose region is no `Owned` leader (a sort's runs, a build) leaves the word. Under
/// PGRUST_GRAVITY_PHASES a pipeline the rule passes over prints why (`EXACT kind=final-skip`).
struct LateSize {
    pipeline: usize,
    producer: usize,
    slot: u8,
    word: usize,
    was: u64,
    count: core::cell::Cell<Option<u64>>,
}

impl LateSize {
    fn of(p: &Proto, cx: &Prepare<'_>, sited: bool) -> Option<LateSize> {
        if crate::inputs::knobs::grant::exact_size() & gravity_driver::EXACT_FINAL == 0 {
            return None;
        }
        let k = p.pipeline as usize;
        let skip = |why: &str| {
            if crate::phases::on() {
                eprintln!("EXACT kind=final-skip pipeline={k} why={why}");
            }
            None
        };
        if p.arg != 1 << Fact::TableSize as u8 {
            return skip("dense");
        }
        if sited {
            return skip("relation");
        }
        let Some(q) = cx.program.pipelines.get(k) else { return skip("pipeline") };
        let (&[slot], gravity_program::ClaimKind::Source) = (q.consumes, q.claims) else { return skip("consumes") };
        let ps = &cx.program.pipelines;
        let producer = match q.consumes_from.first() {
            Some(l) => Some(l.0 as usize).filter(|&j| j < ps.len() && ps[j].produces() == Some(slot)),
            None => (0..k).rev().find(|&j| ps[j].produces() == Some(slot)),
        };
        let Some(producer) = producer else { return skip("producer") };
        let word = p.words.base as usize;
        Some(LateSize { pipeline: k, producer, slot, word, was: cx.words.get(word).copied().unwrap_or(0), count: core::cell::Cell::new(None) })
    }
}

impl Live for LateSize {
    fn answers(&self) -> u16 {
        crate::ops::ask::AFTER | crate::ops::ask::LATE
    }

    fn after_pipeline(&self, pipeline: usize, exec: &mut Exec) -> Option<gravity_driver::Result<()>> {
        if pipeline == self.producer {
            let count = gravity_abi::breaker::Owned::counts(exec.node().slot_region(self.slot)).map(|(records, _)| records);
            self.count.set(count);
            if count.is_none() && crate::phases::on() {
                eprintln!("EXACT kind=final-skip pipeline={} why=no-owned-count producer={}", self.pipeline, self.producer);
            }
            // B4b: the table's share at its open reads the count as its rows (driver `Node::share`), not the 8 MiB floor
            if let Some(n) = count.filter(|&n| n > 0) {
                exec.node_mut().set_late_rows(self.pipeline, n);
            }
        }
        None
    }

    fn late_words(&self, pipeline: usize, out: &mut gravity_parallel::LateWords) -> PgResult<()> {
        let Some(count) = self.count.get().filter(|&n| n > 0 && pipeline == self.pipeline) else { return Ok(()) };
        let size = if self.was == 0 { count } else { self.was.min(count) };
        fires::fire("exact.late_size"); // shapecheck: a slot-fed table sized from its producer's counted records (exactsize B4)
        if crate::phases::on() {
            eprintln!("EXACT kind=final pipeline={} producer={} count={count} was={} size={size}", self.pipeline, self.producer, self.was);
        }
        out.with_vec(|v| v.push((self.word, size, 0))); // alloc: Exec charged out
        Ok(())
    }
}

/// SRC-38: a site without parts' extrema (a heap relation's) bounds its key lane `lane` by the column's statistics: the
/// smallest and largest of ANALYZE's MCV values and histogram bounds, as datum words: bound_lane declares the words only
/// for a key whose image is one integer word, which is its datum word for a by-value type (a numeric's mantissa is by
/// reference: none). A sample's range, so a key past it takes the dense table's overflow (dense_small.rs) or the owner's
/// plain rerun (D597), never a wrong row. None without statistics or a distinct estimate, or when the span passes the
/// distinct values by `ADAPT_FACTOR` (a sparse column).
fn stats_range(cx: &Prepare<'_>, site: usize, lane: usize) -> PgResult<Option<(i64, i64)>> {
    let Some(d) = cx.program.sites.get(site).filter(|d| d.relation != 0 && matches!(d.kind, SiteKind::RangeSource { .. } | SiteKind::IndexSource { .. })) else { return Ok(None) };
    let attno = match d.lanes().get(lane).map(|l| l.id) {
        Some(LaneId::Attr(a)) if a > 0 => a,
        _ => return Ok(None),
    };
    let Some(stats) = syscache_seams::lookup_pg_statistic_bundle::call(cx.mcx, d.relation, attno, false)? else { return Ok(None) };
    let mut range: Option<(i64, i64)> = None;
    for s in stats.slots.iter().filter(|s| s.kind == STATISTIC_KIND_MCV || s.kind == STATISTIC_KIND_HISTOGRAM) {
        let t = s.valuetype()?;
        if !matches!(gravity_fn::typesem::storage::len_byval(t)?, (1 | 2 | 4 | 8, true)) {
            return Ok(None);
        }
        for v in s.values()?.iter().map(|v| v.as_i64()) {
            range = Some(range.map_or((v, v), |(lo, hi)| (lo.min(v), hi.max(v))));
        }
    }
    // the dense open sizes its table at the span (dense_small.rs `open`): a sparse span past the column's distinct values by
    // more than agg_open's bypass factor would set the estimate bypass where a hashed heap table opens unbypassed today
    let distinct = match stats.stadistinct {
        n if n > 0.0 => n as f64,
        n if n < 0.0 => {
            let rel = table::table_open(cx.mcx, d.relation, NoLock)?;
            // C3 S5 (D3889): a baked size statistic (a TRUNCATE's storage-scoped message moves it, not the entry)
            gravity_deps::note_stats(d.relation);
            let t = rel.rd_rel.reltuples;
            rel.close(NoLock)?;
            if t.is_finite() && t > 0.0 { -n as f64 * t as f64 } else { return Ok(None) }
        }
        _ => return Ok(None),
    };
    Ok(range.filter(|&(lo, hi)| (hi.abs_diff(lo) as f64) < ADAPT_FACTOR as f64 * distinct.max(1.0)))
}

/// pg_statistic.h's slot kinds.
const STATISTIC_KIND_MCV: i16 = 1;
const STATISTIC_KIND_HISTOGRAM: i16 = 2;

/// `observe` (table.sized from observation): after pipeline `pipeline`, the groups of its finished table (breaker slot
/// `slot`), summed over the leader region's owner tops, and the rows its driving site handed on (every worker's: the
/// rows the table folded, KEYED-22), remembered process-wide under (the program's content key, the pipeline) for the
/// next execution's TableSize; a stale count sizes a table, never answers. A table that never reached
/// the owner phase, or whose owner tops count no groups, is not remembered: its TableSize keeps the rows / span bound. A
/// grouped pair owner (a DISTINCT fold under group keys, `OwnerDesc::pair`) is never observed (D533): its tops count the
/// accumulator's groups, not the pairs its phase-1 table holds, and a next execution's table sized at the groups laid the
/// pairs at the floor's partitions, each past the owner's table many times over. The plain one (no group keys: `count(DISTINCT x)`
/// alone) remembers its owners' `total`, the deduped pairs, which is what its phase-1 table holds: with no bound that table
/// never asks the estimate's bypass (kernels `agg_open`) and a W8 worker keeps it keyed past its slots. It leaves the
/// runner's own after-pipeline steps (`None`).
struct Observe;

struct ObserveLive {
    key: u128,
    pipeline: usize,
    slot: u8,
    site: Option<usize>,
    pairs: bool,
    /// The execution's `memo_key`, which the observed range is remembered under (0: the owners observe no key).
    memo: u64,
}

impl Protocol for Observe {
    fn kind(&self) -> ProtoKind {
        ProtoKind::Observe
    }
    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        let owner = cx.program.pipelines.get(p.pipeline as usize).and_then(|q| q.owner);
        let pairs = owner.is_some_and(|d| d.pair != 0);
        if pairs && owner.is_some_and(|d| d.acc.nkeys() != 0) {
            return Ok(None);
        }
        let site = cx.program.pipelines.get(p.pipeline as usize).and_then(|q| usize::try_from(q.driving_site).ok());
        let observes = owner.is_some_and(|d| d.observe_key != gravity_program::OBSERVE_NONE);
        let memo = if observes { memo_key(cx, p.pipeline as usize)? } else { 0 };
        Ok(Some(Box::new(ObserveLive { key: cx.program.key.0, pipeline: p.pipeline as usize, slot: p.slot, site, pairs, memo }))) // alloc: Exec
    }
}

impl Live for ObserveLive {
    fn answers(&self) -> u16 {
        crate::ops::ask::AFTER
    }

    fn after_pipeline(&self, pipeline: usize, exec: &mut Exec) -> Option<gravity_driver::Result<()>> {
        let region = exec.node().slot_region(self.slot);
        let counts: fn(&[u8]) -> Option<(u64, Option<u64>)> = if self.pairs { gravity_abi::breaker::Owned::totals } else { gravity_abi::breaker::Owned::counts };
        if let Some((records, groups)) = (pipeline == self.pipeline).then(|| counts(region)).flatten() {
            if let (true, Some(groups)) = (records > 0, groups.filter(|&g| g > 0)) {
                let range = gravity_abi::breaker::Owned::key_range(region).map(|(lo, hi)| (lo, hi, self.memo));
                let rows = self.site.map_or(0, |s| exec.node().site_rows_out(s));
                remember(self.key, pipeline, (groups, rows, range));
            }
        }
        None
    }
}

/// `observe`'s counts, (groups, rows folded, the key range its owners observed with the execution's `memo_key`:
/// SRC-38 slice 2, `Owned::key_range`), process-wide (memo.rs's `GROUPS` rule: cleared whole past `OBSERVED_CAP` keys).
static OBSERVED: pgsync::Mutex<Option<ObservedMap>> = pgsync::Mutex::new(None);

/// `OBSERVED`'s holder, process-wide as the map: the program caches' (the process-wide tier's
/// holder, admit `PROGRAM_CACHES`).
const OBSERVED_OWNER: memledger::Owner = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::ProgramCaches));

/// `OBSERVED`'s map and the charge of its table on `OBSERVED_OWNER` (R1: the charge rides in the struct whose bytes it
/// counts; declared first, as `ChargedVec`'s, so its door-1 drop moves the bytes before the free), refit after each insert
/// (R2) on the inserting thread: a growth's new table is that thread's allocation, its old one that thread's free.
struct ObservedMap {
    charge: memledger::Charge,
    map: HashMap<(u128, usize), Observed>,
}

impl ObservedMap {
    fn new() -> ObservedMap {
        let o = OBSERVED_OWNER;
        ObservedMap { charge: memledger::Charge::open(o, memledger::charge::release_of(o), memledger::Kind::Region, memledger::charge::Door::Heap, 0), map: HashMap::new() }
    }

    /// The table's bytes: its buckets (std's hashbrown: a power of two holding `capacity` at 7/8 load, at most 3 / 7 items
    /// in 4 / 8 buckets) of one entry and one control byte each, plus a control group's tail.
    fn bytes(&self) -> u64 {
        let cap = self.map.capacity();
        if cap == 0 {
            return 0;
        }
        let buckets = if cap < 8 { (cap + 1).next_power_of_two() } else { (cap * 8 / 7).next_power_of_two() };
        (buckets * (core::mem::size_of::<((u128, usize), Observed)>() + 1) + 16) as u64
    }

    fn refit(&mut self) {
        let n = self.bytes();
        self.charge.fit_committed(n);
    }
}

type Observed = (u64, u64, Option<(i64, i64, u64)>);

/// SRC-38 slice 2: what pipeline `pipeline`'s observed key range is remembered and read under, hashed (FNV-1a): the
/// statement's value words (a graph admission's literals, extern parameters and key expressions, with their null flags;
/// none for a shape-table admission; the words a protocol writes, `GraphWord::Zero`, and a late key, NULL until its
/// pipeline starts, are left out, so two executions of one statement agree; D2834: a deferred parameters' list word is in,
/// so an execution reading one never takes another's range), then the pipeline's driving heap's storage:
/// its relfilenode (a TRUNCATE or a rewrite moves it) and its main-fork blocks (rows added, or updated into new pages,
/// move them). A range observed over other words or other storage is not read: that execution stays hashed and observes.
fn memo_key(cx: &Prepare<'_>, pipeline: usize) -> PgResult<u64> {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut fold = |bytes: &[u8]| {
        for &b in bytes {
            h = (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    if let Some(g) = cx.admitted.graph.as_ref() {
        for (i, w) in g.words.iter().enumerate().filter(|(_, w)| matches!(w, crate::admit::GraphWord::Const { .. } | crate::admit::GraphWord::Extern(_) | crate::admit::GraphWord::Expr(_) | crate::admit::GraphWord::Deferred)) {
            let (v, n) = (cx.words.get(i).copied().unwrap_or(0), cx.nulls.get(i).copied().unwrap_or(1));
            fold(&(i as u64).to_le_bytes());
            fold(&v.to_le_bytes());
            fold(&[n]);
        }
    }
    let site = cx.program.pipelines.get(pipeline).and_then(|q| usize::try_from(q.driving_site).ok());
    if let Some((s, d)) = site.and_then(|s| cx.program.sites.get(s).map(|d| (s, d))).filter(|(_, d)| d.relation != 0 && matches!(d.kind, SiteKind::RangeSource { .. } | SiteKind::IndexSource { .. })) {
        // D3585: a relation set's every relation; (D3663) the members its initial pruning keeps, which the words decide
        let kept = cx.initial.and_then(|x| x.set_kept(s));
        let rels = if d.set.is_empty() { core::slice::from_ref(&d.relation) } else { &d.set[..] };
        for (_, &relid) in rels.iter().enumerate().filter(|&(i, _)| kept.is_none_or(|v| crate::ops::append::prune::member(v, i))) {
            let rel = table::table_open(cx.mcx, relid, NoLock)?;
            let number = rel.rd_locator.get().relNumber;
            rel.close(NoLock)?;
            fold(&u64::from(number).to_le_bytes());
            fold(&gravity_hosts::ops::scan::heap_source::nblocks(cx.mcx, relid)?.to_le_bytes());
        }
    }
    Ok(h)
}

/// Sizing feedback for `(key, pipeline)`, controlled by `pgrust.gravity_grant_feedback`.
fn observed(key: u128, pipeline: usize) -> Option<Observed> {
    use gravity_hosts::ops::scan::pgrc::cond_cache::reuse::{note, Kind};
    if !crate::inputs::knobs::grant::feedback_enabled() {
        return None;
    }
    let o = OBSERVED.lock().unwrap_or_else(|e| e.into_inner()).as_ref()?.map.get(&(key, pipeline)).copied();
    note(Kind::Feedback, o.is_some() as u64);
    o
}

fn remember(key: u128, pipeline: usize, counts: Observed) {
    if !crate::inputs::knobs::grant::feedback_enabled() {
        return;
    }
    let mut g = OBSERVED.lock().unwrap_or_else(|e| e.into_inner());
    let m = g.get_or_insert_with(ObservedMap::new);
    if m.map.len() >= OBSERVED_CAP && !m.map.contains_key(&(key, pipeline)) {
        m.map.clear();
    }
    m.map.insert((key, pipeline), counts); // alloc: Cold charged m.charge
    m.refit();
}

#[cfg(test)]
mod tests {
    use super::{observed, remember};
    use crate::inputs::knobs::grant::{set_feedback_for_test, Feedback};
    use gravity_hosts::ops::scan::pgrc::cond_cache::reuse::{census, Kind};
    use guc_tables::ConditionCacheMode;

    #[test]
    fn sizing_feedback_obeys_its_setting_in_every_condition_cache_mode() {
        for mode in [ConditionCacheMode::Off, ConditionCacheMode::Granule, ConditionCacheMode::Rows, ConditionCacheMode::Full] {
            guc_tables::backing::set_pgrust_condition_cache(mode as i32);
            let key = (0x0b5e_12ed_u128 << 64 | 0x5717_c4ed) + mode as u128;
            set_feedback_for_test(Feedback::Off);
            remember(key, 0, (7, 70, None));
            assert_eq!(observed(key, 0), None);
            set_feedback_for_test(Feedback::Memory);
            assert_eq!(observed(key, 0), None, "disabled feedback was not stored");
            let before = census()[Kind::Feedback as usize];
            remember(key, 0, (7, 70, Some((1, 9, 3))));
            assert_eq!(observed(key, 0), Some((7, 70, Some((1, 9, 3)))));
            assert!(census()[Kind::Feedback as usize] > before);
            set_feedback_for_test(Feedback::Off);
            assert_eq!(observed(key, 0), None, "disabled feedback was not read");
            remember(key, 0, (8, 80, None));
            set_feedback_for_test(Feedback::On);
            assert_eq!(observed(key, 0), Some((7, 70, Some((1, 9, 3)))), "disabled feedback did not overwrite history");
            remember(key, 0, (9, 90, None));
            assert_eq!(observed(key, 0), Some((9, 90, None)));
        }
        set_feedback_for_test(Feedback::Memory);
        guc_tables::backing::set_pgrust_condition_cache(ConditionCacheMode::Full as i32);
    }
}

//! M1's keyed and join protocols (plan §4.3): `keyed.twopass`, `join.buildside` and `join.rtfilter`,
//! each over its region reader and hosts v4 m1's builders. Created and closed by M1 (owner M1-HOSTS); m1.rs lists them.
use core::cell::OnceCell;
use std::sync::Arc;

use gravity_driver::{Exec, Merge};
use gravity_hosts::{PartList, VerdictCell, VerdictClaim};
use gravity_hosts::ops::agg::hosts as m1;
use gravity_kernels::table::AdmitHdr;
use gravity_program::protos::{Proto, ProtoKind, RegionFn, NONE, NO_SLOT};
use types_error::PgResult;
use types_storage::lock::NoLock;

use crate::ops::{Live, Prepare, Protocol};
use crate::glue;

use gravity_abi::breaker::MIN_REGION;

/// What the protocols that read a finished build (`join.buildside`'s empty reading, `join.rtfilter`'s and `join.domain`'s
/// key export) ask of the execution (the Node's; a test's stand-in).
pub(crate) trait BuildNode {
    /// D4054: arm `p` finished the production pipeline `of` belongs to (`Node::finishes`): `of` itself, or the arm of
    /// its append.arms run that neither continues nor was stepped past.
    fn finishes(&self, of: usize, p: usize) -> bool;
    /// The slot's spilled generation-0 partitions (0: it never spilled).
    fn spill_parts(&self, slot: u8) -> u32;
    fn region(&mut self, slot: u8) -> &mut [u8];
    /// CONVERSION WAVE 1: the slot's ledger owner (`Node::slot_owner`), which a region reader charges its working memory to.
    fn slot_owner(&self, slot: u8) -> Option<memledger::Owner>;
    /// The producers `mask` names run as if no claim reached them (`Exec::run_empty`).
    fn skip_producers(&mut self, mask: u64);
}

impl BuildNode for Exec {
    fn finishes(&self, of: usize, p: usize) -> bool {
        self.node().finishes(of, p)
    }
    fn spill_parts(&self, slot: u8) -> u32 {
        self.node().spill_parts_of(slot)
    }
    fn region(&mut self, slot: u8) -> &mut [u8] {
        self.node_mut().slot_region(slot)
    }
    fn slot_owner(&self, slot: u8) -> Option<memledger::Owner> {
        self.node().slot_owner(slot)
    }
    fn skip_producers(&mut self, mask: u64) {
        self.run_empty(mask)
    }
}

/// A protocol's reader over the finished region of breaker slot `slot` (at least `MIN_REGION` bytes, else never laid out).
struct Reader {
    peer: usize,
    slot: u8,
    read: RegionFn,
    words: memledger::ChargedVec<u64>,
}

impl Reader {
    /// The reader's words: the proto's words, then `arg` (a lowering-time shape), then, when `arg`'s bits 48..63 are a
    /// kernel word's ordinal + 1 (0 = none), that word as bound for this execution (a literal never rides `arg`).
    fn of(p: &Proto, cx: &Prepare<'_>, what: &str) -> PgResult<Reader> {
        let (Some(read), true) = (p.read[0], p.slot != NO_SLOT && p.peer != NONE) else {
            return Err(glue::internal(&format!("gravity: {what} without its reader or its producing pipeline"))); // alloc: Cold
        };
        let (base, n) = (p.words.base as usize, p.words.n as usize);
        let src = cx.words.get(base..base + n).ok_or_else(|| glue::internal("gravity: a v4 protocol's words past the kernel words"))?;
        let mut words: memledger::ChargedVec<u64> = cx.charged_vec();
        words.with_vec(|v| { v.reserve_exact(n); v.extend_from_slice(src) }); // alloc: Exec charged cx.owner
        words.with_vec(|v| v.push(p.arg)); // alloc: Exec charged cx.owner
        // bits 48..62 (lowerv4 twopass.rs `Rule::pack`; bit 63 is the rule's GT-1 `spread`)
        if let Some(b) = ((p.arg >> 48 & 0x7fff) as usize).checked_sub(1) {
            let bound = *cx.words.get(b).ok_or_else(|| glue::internal("gravity: a reader's bound word past the kernel words"))?;
            words.with_vec(|v| v.push(bound)); // alloc: Exec charged cx.owner
        }
        Ok(Reader { peer: p.peer as usize, slot: p.slot, read, words })
    }

    /// `f` over this reader's reading after pipeline `pipeline`, when that is the producer and its region was laid out (and
    /// the ledger owner of the slot read, `reading`'s).
    fn after<T>(&self, pipeline: usize, exec: &mut Exec, f: impl FnOnce(m1::Reading<'_>, Option<memledger::Owner>) -> T) -> Option<T> {
        if pipeline != self.peer {
            return None;
        }
        self.reading(exec, f)
    }

    /// `f` over this reader's reading of its slot's region, when the region was laid out, with that slot's ledger owner
    /// (`BuildNode::slot_owner`): the RegionFn charges its own working memory to it, and the host `f` calls (hosts agg
    /// hosts.rs, R5: an `(owner, kind)` pair, the kind the caller's by construction) charges its own.
    fn reading<T>(&self, node: &mut dyn BuildNode, f: impl FnOnce(m1::Reading<'_>, Option<memledger::Owner>) -> T) -> Option<T> {
        let owner = node.slot_owner(self.slot);
        let r = node.region(self.slot);
        let region = (r.len() >= MIN_REGION).then_some(&*r)?;
        Some(f(&mut |emit| super::raw::region_read(self.read, region, &self.words, owner, emit), owner))
    }

    /// D4054: pipeline `pipeline` finished the build `peer` names: `peer` itself, or the arm of its append.arms run that
    /// finished the slot. After an arm that continues the run the slot holds only the rows so far, and a reading of it
    /// would export a part of the build's keys as all of them.
    fn built(&self, pipeline: usize, node: &dyn BuildNode) -> bool {
        node.finishes(self.peer, pipeline)
    }

    /// `after` for a build's reader: `f` over the reading after the pipeline that finished the build (`built`).
    fn after_build<T>(&self, pipeline: usize, node: &mut dyn BuildNode, f: impl FnOnce(m1::Reading<'_>, Option<memledger::Owner>) -> T) -> Option<T> {
        if !self.built(pipeline, node) {
            return None;
        }
        self.reading(node, f)
    }

    /// The producer spilled partitions and its region is no finished owner result: such a region holds only what stayed in
    /// memory (D254 grows a region instead), so no reading of it is whole. D3158: a finished `Owned` region is whole even
    /// after a spill, since the owner phase folds the slot's spilled epochs beside the partials (partitioned-record.md
    /// §3.7: section `own_with` and `own_all` read the stream's files), as the runner's `build_admit_set` reads it.
    fn spilled(&self, exec: &Exec) -> bool {
        self.spill_parts(exec) > 0 && !super::raw::owned(exec.node().slot_region(self.slot))
    }

    /// The producer's spilled generation-0 partitions (0: it never spilled).
    fn spill_parts(&self, exec: &Exec) -> u32 {
        exec.node().spill_parts_of(self.slot)
    }
}

/// `keyed.twopass` (H08): pass 1's admitted keys as the exact set pass 2 folds, behind `words[base]` (`AdmitHdr`).
pub(crate) struct TwoPass;

struct TwoPassLive {
    r: Reader,
    hdr: Box<super::raw::IrCell<AdmitHdr>>,
    set: OnceCell<m1::AdmitSet>,
    /// KEYED-02 / EX-15: this execution runs one pass (`having_cost::one_pass`): pass 1 runs whole empty
    /// (`Live::pruned_whole`, D2105) and pass 2 admits every key, the D459 spill path's set.
    one_pass: bool,
    /// 5c-F6: a top-count rule (twopass.count), whose pass 1 past `tie_bail`'s cut admits every key.
    top: bool,
}

impl Protocol for TwoPass {
    fn kind(&self) -> ProtoKind {
        ProtoKind::TwoPass
    }
    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        let hdr = Box::new(super::raw::IrCell::new(AdmitHdr { set: gravity_abi::word(core::ptr::null()), mask: 0, count: 0, empty_key: 0, filter: gravity_abi::word(core::ptr::null()), filter_bits: 0 })); // alloc: Exec
        let base = p.words.base as usize;
        match (p.words.n, cx.words.get_mut(base), cx.nulls.get_mut(base)) {
            (1, Some(w), Some(n)) => (*w, *n) = (hdr.addr(), 0),
            _ => return Err(glue::internal("gravity: keyed.twopass without its header word")),
        }
        let r = Reader::of(p, cx, "keyed.twopass")?;
        let room = having_cost::room(cx.grant.bytes, cx.width);
        // 5c-F6: a top-count rule (lowerv4 twopass.rs `Rule`: no floor, bit 47)
        let top = p.arg & 1 == 0 && p.arg >> 47 & 1 != 0;
        let one_pass = having_cost::one_pass(p.arg, r.words.last().copied(), room);
        let est = p.arg >> 24 & 0x7f_ffff;
        if p.arg & 1 != 0 && est != 0 && crate::inputs::knobs::seam::report_on() {
            let (m, other) = having_cost::unpack(est).unwrap_or_default();
            let bound = r.words.last().copied().filter(|_| p.arg >> 48 & 0x7fff != 0);
            let pass1 = having_cost::pass1_bytes(est).map_or(0, |b| b as u64 >> 10);
            let _ = elog::ereport(types_error::LOG).errmsg(format!("gravity: twopass.having runs {} (mean group {m:.2}, folds {other:.3} reads, bound word {bound:?}, pass 1 {pass1} KiB, room {} KiB)", if one_pass { "one pass" } else { "two passes" }, room >> 10)).errhidestmt(true).errhidecontext(true).finish(types_error::ErrorLocation::new("gravity", 0, "twopass")); // alloc: Cold
        }
        Ok(Some(Box::new(TwoPassLive { r, hdr, set: OnceCell::new(), one_pass, top }))) // alloc: Exec
    }
}

impl TwoPassLive {
    /// 5c-F6: a top-count rule's admitted set `a` past `TWOPASS_TOP_ADMIT_CUT_PCT` of pass 1's records (its owners'
    /// count) is no cut worth a probe a row: pass 2 folds every key (the D459 set); under it, the exact set.
    fn tie_cut(&self, a: m1::AdmitSet, exec: &mut Exec) -> m1::AdmitSet {
        let records = gravity_abi::breaker::Owned::counts(exec.node().slot_region(self.r.slot)).map_or(0, |c| c.0);
        if !tie_bail::past_cut(a.count, records) {
            return a;
        }
        fires::fire("keyed.twopass.tie_bail"); // shapecheck fire signal (5c-F6): pass 1 admitted past the cut, pass 2 folds every key
        if crate::inputs::knobs::seam::report_on() {
            let _ = elog::ereport(types_error::LOG).errmsg(format!("gravity: twopass.count pass 1 admitted {} of {records} records: pass 2 folds every key", a.count)).errhidestmt(true).errhidecontext(true).finish(types_error::ErrorLocation::new("gravity", 0, "twopass")); // alloc: Cold
        }
        m1::AdmitSet::whole(true)
    }
}

impl Live for TwoPassLive {
    fn answers(&self) -> u16 {
        crate::ops::ask::AFTER | crate::ops::ask::PRUNED
    }

    fn pruned_whole(&self, pipeline: usize) -> bool {
        self.one_pass && pipeline == self.r.peer
    }
    fn after_pipeline(&self, pipeline: usize, exec: &mut Exec) -> Option<gravity_driver::Result<()>> {
        // D459: pass 1 spilled partitions and its region is no finished owner result (`Reader::spilled`), so it names no
        // whole set: pass 2 folds every key (the fold PostgreSQL does; the top / HAVING after it cut exactly as over any
        // group). D3158: a pass 1 that spilled into a finished owner phase names its exact set (the owners folded the
        // spilled epochs), so pass 2 stays two-pass. KEYED-02: a pass 1 this execution's choice skipped folds every key
        // too (it ran whole empty: its region, if laid out, holds no key)
        let spilled = pipeline == self.r.peer && (self.one_pass || self.r.spilled(exec));
        let parts = if pipeline == self.r.peer && !self.one_pass { self.r.spill_parts(exec) } else { 0 };
        if parts > 0 {
            if !spilled {
                fires::fire("keyed.twopass.spilled_owned"); // shapecheck fire signal (D3158): a spilled pass 1's exact set read from its owner phase
            }
            if crate::inputs::knobs::seam::report_on() {
                let _ = elog::ereport(types_error::LOG).errmsg(format!("gravity: keyed.twopass pass 1 spilled {parts} partitions: {}", if spilled { "no owner result, pass 2 admits every key (D459)" } else { "the owner phase's exact set (D3158)" })).errhidestmt(true).errhidecontext(true).finish(types_error::ErrorLocation::new("gravity", 0, "twopass")); // alloc: Cold
            }
        }
        let a = match if spilled { None } else { self.r.after(pipeline, exec, |read, o| m1::admit_set(read, o.map(|o| (o, memledger::Kind::HashAgg)))) } {
            _ if spilled => Some(m1::AdmitSet::whole(true)),
            Some(Ok(a)) => a,
            Some(Err(e)) => return Some(Err(gravity_hosts::stash(e))),
            None if pipeline == self.r.peer => Some(m1::AdmitSet::whole(false)),
            None => return None,
        };
        let Some(a) = a else { return Some(Err(gravity_driver::Error::host("gravity: keyed.twopass could not read pass 1's result"))) };
        let a = if self.top && pipeline == self.r.peer && !spilled && tie_bail::on() { self.tie_cut(a, exec) } else { a };
        let hdr = AdmitHdr { set: gravity_abi::word(a.set.as_ptr()), mask: a.mask, count: a.count, empty_key: a.empty_key, filter: gravity_abi::word(a.filter.as_ptr()), filter_bits: a.filter_bits };
        if self.set.set(a).is_err() {
            return Some(Err(gravity_driver::Error::host("gravity: keyed.twopass's pass 1 finished twice")));
        }
        self.hdr.set(hdr);
        Some(Ok(()))
    }
}

/// `twopass.floor` (D3157): the bare pass 1's shared floor of the top's `arg` best keys (kernels fold_entries.rs
/// `PASS1_SHARED_*`) behind `words[base]`, laid zeroed per execution; its claims merge into it at their ends.
pub(crate) struct TwoPassFloor;

struct TwoPassFloorLive {
    pipeline: usize,
    words: Box<[core::sync::atomic::AtomicU64]>,
}

impl Protocol for TwoPassFloor {
    fn kind(&self) -> ProtoKind {
        ProtoKind::TwoPassFloor
    }
    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        let n = p.arg;
        if n == 0 || n > gravity_program::cost::TWOPASS_SHARED_KEYS_MAX {
            return Err(glue::internal("gravity: twopass.floor over no top or past its key cap"));
        }
        let words: Box<[core::sync::atomic::AtomicU64]> = (0..gravity_kernels::host_abi::pass1_shared_words(n)).map(|_| core::sync::atomic::AtomicU64::new(0)).collect(); // alloc: Exec
        let base = p.words.base as usize;
        match (p.words.n, cx.words.get_mut(base), cx.nulls.get_mut(base)) {
            (1, Some(w), Some(z)) => (*w, *z) = (words.as_ptr() as u64, 0),
            _ => return Err(glue::internal("gravity: twopass.floor without its handle word")),
        }
        *cx.nparams = (*cx.nparams).max(base + 1);
        Ok(Some(Box::new(TwoPassFloorLive { pipeline: p.pipeline as usize, words }))) // alloc: Exec
    }
}

impl Live for TwoPassFloorLive {
    fn answers(&self) -> u16 {
        crate::ops::ask::REOPEN
    }

    /// A rerun set re-opened over pass 1: its next run starts from an empty floor (a key of the last run's is no bound on
    /// this one's). On the leader, no claim running.
    fn reopen(&self, members: &[u16]) -> PgResult<()> {
        if members.contains(&(self.pipeline as u16)) {
            self.words.iter().for_each(|w| w.store(0, core::sync::atomic::Ordering::Relaxed));
        }
        Ok(())
    }
}

/// `join.buildside` (H18c): the build pipeline's merge is the v4 build's (`MergeImpl` under `KindId::JoinInner`).
pub(crate) struct BuildSide;

struct BuildSideLive {
    pipeline: usize,
    merge: &'static dyn Merge,
    /// D276: the build's slot, its region reader (`read[0]`: one `(0, N, FORM)`), and the probe's outer-side producers
    /// (`arg` bits 8..47 = pipelines 0..39) PG never runs over an empty build on the build-first path.
    slot: u8,
    read: Option<RegionFn>,
    arg: u64,
}

impl Protocol for BuildSide {
    fn kind(&self) -> ProtoKind {
        ProtoKind::BuildSide
    }
    fn prepare(&self, p: &Proto, _cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        use gravity_lowerv4::face::{end::MergeImpl, kinds::KindId};
        let Some(m) = gravity_lowerv4::ops::kind_as::<MergeImpl>(KindId::JoinInner) else {
            return Err(glue::internal("gravity: join.buildside without the build's merge"));
        };
        Ok(Some(Box::new(BuildSideLive { pipeline: p.pipeline as usize, merge: m.0, slot: p.slot, read: p.read[0], arg: p.arg }))) // alloc: Exec
    }
}

impl Live for BuildSideLive {
    fn answers(&self) -> u16 {
        crate::ops::ask::MERGE | crate::ops::ask::AFTER
    }

    /// The build's merge; with the DEFER bit (`arg` bit 0, op 14) its variant that holds the build's error for the probe
    /// to raise (`Merge::with_defer`), when the merge has one.
    fn merge_for(&self, pipeline: usize) -> Option<&dyn Merge> {
        let defer = self.arg & 1 != 0;
        (pipeline == self.pipeline).then(|| if defer { self.merge.with_defer(true).unwrap_or(self.merge) } else { self.merge })
    }

    /// D276: after the build, an empty one (its reading's N 0; a spilled build is never read as empty) runs the masked
    /// outer-side producers as if no claim reached them (`Exec::run_empty`). The runner's own steps still run (`None`).
    fn after_pipeline(&self, pipeline: usize, exec: &mut Exec) -> Option<gravity_driver::Result<()>> {
        self.read_empty(pipeline, exec);
        None
    }
}

impl BuildSideLive {
    /// D276's reading after pipeline `pipeline`. D4054: only after the pipeline that finished the build (`Node::finishes`:
    /// the build's own, or the arm of its append.arms run that finished the slot). An arm that continues the run leaves
    /// a slot the later arms still append to: read then, a build whose rows all come later reads as empty.
    fn read_empty(&self, pipeline: usize, node: &mut dyn BuildNode) -> Option<()> {
        let mask = (self.arg >> 8) & ((1 << 40) - 1);
        let read = self.read.filter(|_| mask != 0 && node.finishes(self.pipeline, pipeline) && node.spill_parts(self.slot) == 0)?;
        let owner = node.slot_owner(self.slot);
        let r = node.region(self.slot);
        let region = (r.len() >= MIN_REGION).then_some(&*r)?;
        let mut n = None;
        super::raw::region_read(read, region, &[self.arg], owner, &mut |_, rows, _| n = Some(rows))?;
        if n == Some(0) {
            node.skip_producers(mask);
        }
        None
    }
}

/// `join.rtfilter` (H07c): after build `peer`, its keys (hosts v4 m1 `rt_filter`) arm a runtime filter of probe site
/// `site` on column `arg`; the source measures whether it cuts granules. A site without a pgrc recipe takes none.
pub(crate) struct RtFilter;

struct RtFilterLive {
    r: Reader,
    attno: i16,
    /// D3245: the most runs of keys the filter keeps (the probe's granules, `arg` bits 16..48), past them its range alone.
    cap: usize,
    /// D3245: the probe relation's parts for this snapshot (road join.rtfilter.multi on): keys past `cap` are clipped to its
    /// column's key range (`part_extrema`) before the cap.
    list: Option<Arc<PartList>>,
    cell: Arc<VerdictCell>,
    /// The slot of the site's filters this one claimed.
    slot: usize,
    /// D294: this execution's claim on its slot of the site's runtime filters (the cell is the Admitted's: another live
    /// execution contests it, and the last claim's end clears it, so no later execution reads this one's build keys).
    /// D3245: the site's k-th join.rtfilter claims slot k (`RT_SLOTS` a site; one past them claims none); with road
    /// join.rtfilter.multi off (`ARG_ONE`) the site's first alone claims and arms.
    claim: Option<VerdictClaim>,
}

/// D3245: `arg` bit 63, set by the lowering when road join.rtfilter.multi is off: the site's first filter alone arms, and
/// its keys stay a set within the cap (lowerv4 ops/hashjoin strategies/rtfilter.rs `ARG_ONE`).
const ARG_ONE: u64 = 1 << 63;

impl Protocol for RtFilter {
    fn kind(&self) -> ProtoKind {
        ProtoKind::RtFilter
    }
    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        let r = Reader::of(p, cx, "join.rtfilter")?;
        if p.words.n == 1 {
            return Domain::prepare(p, r, cx); // D3148: the row-level form, a keyed hash end's admitted set
        }
        let Some(g) = usize::try_from(p.site).ok().and_then(|s| cx.admitted.registry.granule_recipe(s)) else { return Ok(None) };
        let nth = cx.program.v4.as_ref().and_then(|d| d.protos.iter().filter(|q| q.kind == ProtoKind::RtFilter && q.words.n != 1 && q.site == p.site).position(|q| (q.peer, q.slot) == (p.peer, p.slot)));
        let slots = if p.arg & ARG_ONE != 0 { 1 } else { gravity_hosts::RT_SLOTS };
        let slot = nth.filter(|&k| k < slots);
        let claim = slot.map(|k| g.verdicts.claim_rt_at(k));
        let cap = ((p.arg >> 16) as u32 as usize).max(gravity_program::cost::RTFILTER_SET_MAX);
        let list = if claim.is_some() && p.arg & ARG_ONE == 0 {
            let rel = table::table_open(cx.mcx, g.rel, NoLock)?;
            let list = gravity_hosts::resolve(&rel, Some(cx.snap));
            rel.close(NoLock)?;
            Some(list?)
        } else {
            None
        };
        Ok(Some(Box::new(RtFilterLive { r, attno: p.arg as i16, cap, list, cell: Arc::clone(&g.verdicts), slot: slot.unwrap_or(0), claim }))) // alloc: Exec
    }
}

impl Live for RtFilterLive {
    fn answers(&self) -> u16 {
        crate::ops::ask::AFTER | crate::ops::ask::REOPEN
    }

    fn after_pipeline(&self, pipeline: usize, exec: &mut Exec) -> Option<gravity_driver::Result<()>> {
        // the owner of the site's rt claim alone reads its build's keys (D294). D2856 (colskip, D3138): after EVERY run of
        // the build its filter replaces the last run's, or clears it when this run gives none (a spilled build, a region
        // never laid out): a rerun set's probe never skips by an earlier run's keys
        // D4054: "after the build" is after the pipeline that finished it (`Reader::built`)
        let built = self.r.built(pipeline, exec);
        if built && self.claim.as_ref().is_some_and(VerdictClaim::owner) {
            let clip = || self.list.as_deref().and_then(|l| gravity_hosts::part_extrema(l, self.attno as u32).ok().flatten());
            match (self.r.spill_parts(exec) > 0, self.r.after_build(pipeline, exec, |read, o| m1::rt_filter(self.attno, self.cap, &clip, read, o.map(|o| (o, memledger::Kind::HashBuild))))) {
                (false, Some(Some(f))) => self.cell.arm_rt_at(self.slot, f),
                _ => self.cell.clear_rt_at(self.slot),
            }
        }
        built.then_some(Ok(()))
    }

    /// D2856 (colskip, D3138): a rerun set re-opened over the build (a run starts): no filter until this run's build arms
    /// its keys, so a probe that seeks before the build runs again (a lazy build, D824) never reads the last run's. A set
    /// that keeps the build (D1018 item 9 b: it runs at the first run only) does not list it, and its filter stays.
    fn reopen(&self, members: &[u16]) -> PgResult<()> {
        if members.contains(&(self.r.peer as u16)) && self.claim.as_ref().is_some_and(VerdictClaim::owner) {
            self.cell.clear_rt_at(self.slot);
        }
        Ok(())
    }
}

/// D3148 (join.domain, lowerv4 ops/hashjoin strategies/domain.rs): join.rtfilter's row-level form. After every run of
/// the build `peer`, its part-0 keys (the proto's reader) are laid as keyed.twopass's admitted set behind `words[base]`
/// (`AdmitHdr`), which the keyed hash end of `pipeline` probes before it folds a row. Every key is admitted (D459's
/// set) before the build's first run, after a run that gives no set (a spilled build, op 14's held error, a build past
/// the cap) or whose set the statement's grant cannot hold, and from a re-opened rerun set's start until the build runs
/// again: the per-run rule colskip's D3138 gives the granule filter. The set is held against the grant
/// (`Reservation::hold`, as a top-k copy or a handed-off table is) and given back when the next run replaces it or the
/// execution ends. Laid on the leader between pipelines: no participant reads it meanwhile.
struct Domain {
    r: Reader,
    hdr: Box<super::raw::IrCell<AdmitHdr>>,
    set: core::cell::RefCell<(m1::AdmitSet, Option<gravity_driver::mem::grant::Held>)>,
}

impl Domain {
    fn prepare(p: &Proto, r: Reader, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        let hdr = Box::new(super::raw::IrCell::new(AdmitHdr { set: gravity_abi::word(core::ptr::null()), mask: 0, count: 0, empty_key: 0, filter: gravity_abi::word(core::ptr::null()), filter_bits: 0 })); // alloc: Exec
        let base = p.words.base as usize;
        match (cx.words.get_mut(base), cx.nulls.get_mut(base)) {
            (Some(w), Some(n)) => (*w, *n) = (hdr.addr(), 0),
            _ => return Err(glue::internal("gravity: join.domain without its header word")),
        }
        let live = Domain { r, hdr, set: core::cell::RefCell::new((m1::AdmitSet::whole(true), None)) };
        live.lay(m1::AdmitSet::whole(true), None);
        Ok(Some(Box::new(live))) // alloc: Exec
    }

    /// Lays `a` (held by `held`) behind the header, then drops the set it replaces.
    fn lay(&self, a: m1::AdmitSet, held: Option<gravity_driver::mem::grant::Held>) {
        self.hdr.set(AdmitHdr { set: gravity_abi::word(a.set.as_ptr()), mask: a.mask, count: a.count, empty_key: a.empty_key, filter: gravity_abi::word(a.filter.as_ptr()), filter_bits: a.filter_bits });
        drop(self.set.replace((a, held)));
    }
}

impl Live for Domain {
    fn answers(&self) -> u16 {
        crate::ops::ask::AFTER | crate::ops::ask::REOPEN
    }

    fn after_pipeline(&self, pipeline: usize, exec: &mut Exec) -> Option<gravity_driver::Result<()>> {
        // D4054: after the pipeline that finished the build (`Reader::built`)
        if !self.r.built(pipeline, exec) {
            return None;
        }
        crate::phases::mark("domain.build"); // PHASES: the leader's serial lay-out of the set is `domain.laid`'s delta
        let a = match if self.r.spilled(exec) { None } else { self.r.after_build(pipeline, exec, |read, o| m1::admit_set(read, o.map(|o| (o, memledger::Kind::HashBuild)))) } {
            Some(Err(e)) => return Some(Err(gravity_hosts::stash(e))),
            Some(Ok(a)) => a,
            None => None,
        };
        // the set's bytes held against the statement's grant (G's room when admitted); refused: every key admitted. Grow
        // mode (D4559): G's room short, the set grows past G from the pool's room now (`hold_growing`), refused only there
        let res = exec.reservation();
        let bytes = a.as_ref().map_or(0, |a| 8 * (a.set.len() + a.filter.len()) as u64);
        let held = a.as_ref().and_then(|_| (!res.is_strict() && (res.grant_ctx().is_none() || res.grant.grow || bytes <= res.left())).then(|| res.hold_growing(bytes)).flatten());
        crate::phases::mark("domain.laid");
        if crate::inputs::knobs::seam::report_on() {
            let n = a.as_ref().map_or(0, |a| a.count);
            let _ = elog::ereport(types_error::LOG).errmsg(format!("gravity: join.domain laid {n} keys ({bytes} bytes, held {})", held.is_some())).errhidestmt(true).errhidecontext(true).finish(types_error::ErrorLocation::new("gravity", 0, "join.domain")); // alloc: Cold
        }
        match (a, held) {
            (Some(a), Some(h)) => {
                fires::fire("rtfilter.domain_armed"); // shapecheck fire signal (D3148): a build's keys admit a keyed end's rows
                self.lay(a, Some(h));
            }
            _ => {
                fires::fire("rtfilter.domain_all"); // shapecheck fire signal (D3148): no set this run, every key admitted
                self.lay(m1::AdmitSet::whole(true), None);
            }
        }
        Some(Ok(()))
    }

    /// A rerun set re-opened over the build (a run starts): every key admitted until this run's build lays its set.
    fn reopen(&self, members: &[u16]) -> PgResult<()> {
        if members.contains(&(self.r.peer as u16)) {
            self.lay(m1::AdmitSet::whole(true), None);
        }
        Ok(())
    }
}

/// 5c-F6: a top-count split (twopass.count) whose pass 1 admits past `TWOPASS_TOP_ADMIT_CUT_PCT` of its records
/// finishes as one pass: pass 2 folds every key. Both forms give the same answers: the bail moves only time.
pub(crate) mod tie_bail {
    use core::sync::atomic::{AtomicU8, Ordering::Relaxed};
    use gravity_program::cost::TWOPASS_TOP_ADMIT_CUT_PCT;

    /// `PGRUST_GRAVITY_TIE_BAIL` (read once a process): on by default; 0 runs both passes over the exact set. A kill
    /// switch for diagnosis only, not a tuning knob: no shipped config sets it.
    pub(crate) fn on() -> bool {
        static MODE: AtomicU8 = AtomicU8::new(u8::MAX);
        match MODE.load(Relaxed) {
            u8::MAX => {
                let m = gravity_driver::env::num::<u8>("PGRUST_GRAVITY_TIE_BAIL").unwrap_or(1).min(1);
                MODE.store(m, Relaxed);
                m != 0
            }
            m => m != 0,
        }
    }

    /// Whether `admitted` of `records` is past the cut (no records counted: never).
    pub(crate) fn past_cut(admitted: u64, records: u64) -> bool {
        records > 0 && admitted as u128 * 100 > records as u128 * TWOPASS_TOP_ADMIT_CUT_PCT as u128
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_cut_is_a_share_of_the_records() {
            assert!(!past_cut(70, 100) && past_cut(71, 100), "past {TWOPASS_TOP_ADMIT_CUT_PCT} %");
            assert!(!past_cut(5, 0), "no records counted: never");
            assert!(past_cut(u64::MAX, u64::MAX / 2) && !past_cut(u64::MAX / 2, u64::MAX), "no overflow");
        }
    }
}

/// KEYED-02 / EX-15: the run-time choice of `twopass.having` (a count floor `count(*) > K` / `>= K` over a group whose
/// pass 1 counts each key and whose pass 2 folds the other aggregates of the keys the floor keeps). The model the entry
/// note names (the perf back stop reads the choice by it): a group's row count is 1 + Poisson(m - 1), `m` the planner's
/// mean group size rows / groups, so a row sits in a group the floor drops with the size-biased share `dropped_share`;
/// two passes read the input twice and count every row, then fold the count and the other aggregates over the kept share
/// only, where one pass folds everything once. K is this execution's bound word, so the choice is made per execution.
pub(crate) mod having_cost {
    /// P(Poisson(`mu`) <= `t`); 0 for `t` < 0. Summed in log space below `mu` = 1000, the normal tail above.
    pub(crate) fn poisson_cdf(mu: f64, t: i64) -> f64 {
        if t < 0 {
            return 0.0;
        }
        if mu <= 0.0 {
            return 1.0;
        }
        if mu > 1000.0 {
            return normal_cdf((t as f64 + 0.5 - mu) / mu.sqrt());
        }
        if t as f64 >= mu + 12.0 * mu.sqrt() + 30.0 {
            return 1.0;
        }
        let (mut ln_term, mut sum) = (-mu, 0.0f64);
        for i in 0..=t {
            if i > 0 {
                ln_term += mu.ln() - (i as f64).ln();
            }
            sum += ln_term.exp();
        }
        sum.min(1.0)
    }

    /// The standard normal CDF (Abramowitz and Stegun 7.1.26's erf, |error| < 1.5e-7).
    fn normal_cdf(z: f64) -> f64 {
        let x = z.abs() / core::f64::consts::SQRT_2;
        let t = 1.0 / (1.0 + 0.327_591_1 * x);
        let poly = t * (0.254_829_592 + t * (-0.284_496_736 + t * (1.421_413_741 + t * (-1.453_152_027 + t * 1.061_405_429))));
        let erf = 1.0 - poly * (-x * x).exp();
        if z >= 0.0 { 0.5 * (1.0 + erf) } else { 0.5 * (1.0 - erf) }
    }

    /// The share of the input's rows in groups the floor drops: counts up to K when `gt` (`count(*) > K`), up to K - 1
    /// otherwise (`>= K`), group counts 1 + Poisson(m - 1). With X = 1 + Y, the rows of groups of count <= t are
    /// sum (1 + y) P(Y = y) over y <= t - 1, which is P(Y <= t - 1) + mu P(Y <= t - 2), over the mean 1 + mu.
    pub(crate) fn dropped_share(m: f64, k: i64, gt: bool) -> f64 {
        let t = if gt { k } else { k.saturating_sub(1) };
        if t < 1 || !m.is_finite() {
            return 0.0;
        }
        let mu = (m - 1.0).max(0.0);
        ((poisson_cdf(mu, t - 1) + mu * poisson_cdf(mu, t - 2)) / (1.0 + mu)).clamp(0.0, 1.0)
    }

    /// The share of the input's rows in groups a count bound drops: `dropped_share` for a floor; for a ceiling
    /// (`below`: `count(*) < K`, `<= K` when not `gt`) the rows of the groups over it, all but those of counts up to K - 1
    /// (up to K for `<=`).
    pub(crate) fn dropped(m: f64, k: i64, gt: bool, below: bool) -> f64 {
        if below { 1.0 - dropped_share(m, k, !gt) } else { dropped_share(m, k, gt) }
    }

    /// Whether two passes cost less than one, per input row: `read` the scan's, `count` the count fold's, `probe` pass 2's
    /// admission probe's, `other` the other aggregates' folds together. Two passes: 2 read + count + probe + (count +
    /// other) x (1 - f); one pass: read + count + other; so two pay when (count + other) x f > read + count + probe.
    pub(crate) fn two_passes_pay(m: f64, k: i64, gt: bool, below: bool, read: f64, count: f64, probe: f64, other: f64) -> bool {
        (count + other) * dropped(m, k, gt, below) > read + count + probe
    }

    // Per input row, in units of one scan read: the count fold and pass 2's admission probe (the cost home's, fitted with
    // lowerv4's fold weights).
    use gravity_program::cost::{HAVING_COUNT as COUNT, HAVING_PROBE as PROBE};

    /// The estimate lowerv4's having split packs into a floor rule's free `n` bits (`arg` bits 24..46; lowerv4
    /// ops/sort/topn/strategies/twopass.rs `having_estimate`): bits 0..7 the mean group size `m` as `1 + round(4 log2 m)`
    /// (0 = none, or a bound on another final than a count, whose share the model does not price: two passes), bits 8..15 the other folds' weight in eighths of a read, bits 16..22
    /// pass 1's table in bytes as `round(2 log2 bytes)` (`pass1_bytes`).
    pub(crate) fn unpack(n: u64) -> Option<(f64, f64)> {
        let q = n & 0xff;
        (q != 0).then(|| (((q - 1) as f64 / 4.0).exp2(), (n >> 8 & 0xff) as f64 / 8.0))
    }

    /// One participant's share of the execution's grant (`grant` bytes over `width` participants, the leader alone at 0):
    /// each participant counts its own rows into its own pass 1 table, which holds up to every key, so a table that
    /// outgrows one share spills.
    pub(crate) fn room(grant: u64, width: u16) -> u64 {
        grant / width.max(1) as u64
    }

    /// Pass 1's table as the estimate bits `n` carry it (bits 16..22, `round(2 log2 bytes)`); `None`: no size estimated.
    pub(crate) fn pass1_bytes(n: u64) -> Option<f64> {
        let b = n >> 16 & 0x7f;
        (b != 0).then(|| (b as f64 / 2.0).exp2())
    }

    /// Whether this execution of a `keyed.twopass` proto (`arg`, `bound` its bound word as this execution binds it,
    /// `room` one participant's share of the grant: `room()`) runs one pass: a floor rule whose pass 1 table outgrows the
    /// room (that pass 1 spills: its I/O, and with no owner result D459's every key, so pass 2 folds every row after the
    /// second read), or a count bound carrying the mean group size whose bound spares less than the second pass costs.
    /// Every other rule (a top, the extreme top, a bound on another final) runs its two passes.
    pub(crate) fn one_pass(arg: u64, bound: Option<u64>, room: u64) -> bool {
        let bit = |i: u32| arg >> i & 1 != 0;
        let (floor, gt, int4, below) = (bit(0), bit(1), bit(2), bit(63));
        let n = arg >> 24 & 0x7f_ffff;
        let Some(raw) = bound.filter(|_| floor && arg >> 48 & 0x7fff != 0) else {
            return false;
        };
        if pass1_bytes(n).is_some_and(|b| b > room as f64) {
            return true;
        }
        let Some((m, other)) = unpack(n) else {
            return false;
        };
        let k = if int4 { raw as u32 as i32 as i64 } else { raw as i64 };
        !two_passes_pay(m, k, gt, below, 1.0, COUNT, PROBE, other)
    }

    #[cfg(test)]
    mod tests {
        use super::{dropped, dropped_share, one_pass, pass1_bytes, poisson_cdf, room, two_passes_pay, unpack};

        /// A grant no table in these tests outgrows.
        const ROOM: u64 = u64::MAX;

        /// A floor rule's arg (`count(*) > K` over an int4 literal, bound word 3) carrying the estimate bits `n`.
        fn floor_arg(n: u64) -> u64 {
            0b111 | (n & 0x7f_ffff) << 24 | 4 << 48
        }

        #[test]
        fn a_floor_rule_runs_one_pass_where_its_floor_spares_too_little() {
            assert_eq!(unpack(0), None);
            assert_eq!(unpack(1 | 64 << 8), Some((1.0, 8.0)));
            assert_eq!(unpack(41), Some((1024.0, 0.0)));
            // near-unique keys (m 1) under count(*) > 1 with costly folds: two passes
            assert!(!one_pass(floor_arg(1 | 64 << 8), Some(1), ROOM));
            // groups of about 1024 rows clear count(*) > 1: one pass
            assert!(one_pass(floor_arg(41 | 64 << 8), Some(1), ROOM));
            // ... but K is this execution's: a floor of -1 as an int4 word (0xffff_ffff) keeps every group too
            assert!(one_pass(floor_arg(41 | 64 << 8), Some(0xffff_ffff), ROOM));
            // no estimate, no bound word, a top rule: the two passes as before
            assert!(!one_pass(floor_arg(0), Some(1), ROOM));
            // a bound on another final carries no mean (bits 0..7 zero): two passes, unless pass 1 would spill
            assert!(!one_pass(floor_arg(64 << 8) | 1 << 63, Some(1), ROOM));
            assert!(one_pass(floor_arg(64 << 8 | 50 << 16) | 1 << 63, Some(1), 8 << 20));
            // a count ceiling over groups of about 1024 rows: count(*) < 2 drops every row (two passes), < 5000 none (one)
            assert!(!one_pass(floor_arg(41 | 64 << 8) | 1 << 63, Some(2), ROOM));
            assert!(one_pass(floor_arg(41 | 64 << 8) | 1 << 63, Some(5000), ROOM));
            assert!(!one_pass(floor_arg(41 | 64 << 8) & !(0xffff << 48), Some(1), ROOM));
            assert!(!one_pass(floor_arg(41 | 64 << 8) & !1, Some(1), ROOM));
        }

        #[test]
        fn a_floor_rule_runs_one_pass_where_pass_1_would_spill() {
            // near-unique keys whose pass 1 table (2^25 bytes: bucket 50) outgrows an 8 MiB grant: one pass; in 64 MiB, two
            let n = 1 | 64 << 8 | 50 << 16;
            assert_eq!(pass1_bytes(n), Some((1u64 << 25) as f64));
            assert_eq!(pass1_bytes(1 | 64 << 8), None);
            assert!(one_pass(floor_arg(n), Some(1), 8 << 20));
            assert!(!one_pass(floor_arg(n), Some(1), 64 << 20));
            // a top rule never reads it
            assert!(!one_pass(floor_arg(n) & !1, Some(1), 8 << 20));
            // each participant's share: 64 MiB over 8 is 8 MiB, the leader alone takes the whole
            assert_eq!(room(64 << 20, 8), 8 << 20);
            assert_eq!(room(64 << 20, 0), 64 << 20);
        }

        #[test]
        fn the_poisson_cdf_sums_its_terms_and_takes_the_normal_tail_past_a_thousand() {
            assert!((poisson_cdf(2.0, 2) - 5.0 * (-2.0f64).exp()).abs() < 1e-12);
            assert_eq!(poisson_cdf(2.0, -1), 0.0);
            assert_eq!(poisson_cdf(0.0, 0), 1.0);
            assert_eq!(poisson_cdf(3.0, 500), 1.0);
            assert!((poisson_cdf(10_000.0, 10_000) - 0.5).abs() < 0.01);
            assert!(poisson_cdf(10_000.0, 9_700) < 0.01 && poisson_cdf(10_000.0, 10_300) > 0.99);
        }

        #[test]
        fn a_floor_drops_the_rows_of_the_groups_at_or_under_it() {
            // every group one row: count(*) > 1 drops them all, count(*) >= 1 none
            assert_eq!(dropped_share(1.0, 1, true), 1.0);
            assert_eq!(dropped_share(1.0, 1, false), 0.0);
            assert_eq!(dropped_share(40.0, 0, true), 0.0);
            // count(*) >= K drops what count(*) > K - 1 drops
            assert_eq!(dropped_share(3.0, 4, false), dropped_share(3.0, 3, true));
            // more rows dropped as the floor rises, all of them far past the mean
            let s: Vec<f64> = (1..12).map(|k| dropped_share(4.0, k, true)).collect();
            assert!(s.windows(2).all(|w| w[0] <= w[1]), "{s:?}");
            assert!(dropped_share(4.0, 200, true) > 0.999_999);
            // mean 2 (Y ~ Poisson(1)), count(*) > 1: the singletons' rows, P(Y = 0) / 2
            assert!((dropped_share(2.0, 1, true) - (-1.0f64).exp() / 2.0).abs() < 1e-12);
        }

        #[test]
        fn two_passes_pay_where_the_floor_spares_the_costly_folds() {
            // near-unique keys under count(*) > 1 spare almost every text min / max fold
            assert!(two_passes_pay(1.1, 1, true, false, 1.0, 0.5, 0.5, 8.0));
            // ... but not one cheap fold: the second read costs more than it spares
            assert!(!two_passes_pay(1.1, 1, true, false, 1.0, 0.5, 0.5, 0.5));
            // large groups clear a low floor: nothing to spare
            assert!(!two_passes_pay(50.0, 1, true, false, 1.0, 0.5, 0.5, 8.0));
            // a floor every group fails drops everything: two passes pay once the other folds cost more than the read and the probe
            assert!(two_passes_pay(5.0, 1_000, true, false, 1.0, 0.5, 0.5, 2.0));
            // a ceiling drops the groups over it: large groups under count(*) < 3 drop almost every row, near-unique keys
            // under count(*) <= 1 keep most, and `< K` keeps what `<= K - 1` keeps
            assert!(two_passes_pay(50.0, 3, true, true, 1.0, 0.5, 0.5, 2.0));
            assert!(!two_passes_pay(1.1, 1, false, true, 1.0, 0.5, 0.5, 8.0));
            assert_eq!(dropped(4.0, 5, true, true), dropped(4.0, 4, false, true));
            assert!((dropped(4.0, 5, false, true) + dropped(4.0, 5, true, false) - 1.0).abs() < 1e-12);
        }
    }
}

#[cfg(test)]
mod build_tests {
    use gravity_driver::Merge;
    use gravity_hosts::ops::agg::hosts as m1;

    use super::{BuildNode, BuildSideLive, Reader};

    /// A build's append.arms run of two arms over slot 0: `Q`, the pipeline that ends in the build, then `P`, its pair
    /// pass, which appends the rows it reads back from a spilled build's partitions.
    const Q: usize = 1;
    const P: usize = 2;
    /// The producers an empty build lets the runner skip (D276's mask).
    const MASK: u64 = 0b11000;

    /// The run as the execution answers for it (`BuildNode`), and what the protocols did to it.
    struct Run {
        /// The build's region: word 0 its rows, then each row's key (raw.rs `test_rows`, `test_keys`).
        region: Vec<u8>,
        /// The run ended at `Q` (`Node::arm_ends_run`): the pair pass has no row and is stepped past.
        ended: bool,
        /// A build of one pipeline (`Q`), no run.
        lone: bool,
        emptied: u64,
    }

    impl Run {
        fn new(ended: bool) -> Run {
            Run { region: vec![0; 256], ended, lone: false, emptied: 0 } // alloc: Cold
        }

        /// An arm appends `keys` to the build.
        fn append(&mut self, keys: &[i64]) {
            let word = |r: &[u8], i: usize| u64::from_ne_bytes(r[8 * i..8 * i + 8].try_into().unwrap());
            let n = word(&self.region, 0) as usize;
            for (i, k) in keys.iter().enumerate() {
                self.region[8 * (n + 1 + i)..8 * (n + 2 + i)].copy_from_slice(&k.to_ne_bytes());
            }
            self.region[..8].copy_from_slice(&((n + keys.len()) as u64).to_ne_bytes());
        }
    }

    impl BuildNode for Run {
        /// `Node::finishes`: a lone build's own pipeline; of the run, its first arm when it ended there, else its last.
        fn finishes(&self, of: usize, p: usize) -> bool {
            match self.lone {
                true => p == of,
                false => [Q, P].contains(&of) && p == if self.ended { Q } else { P },
            }
        }
        fn spill_parts(&self, _slot: u8) -> u32 {
            0
        }
        fn region(&mut self, _slot: u8) -> &mut [u8] {
            &mut self.region
        }
        fn slot_owner(&self, _slot: u8) -> Option<memledger::Owner> {
            None
        }
        fn skip_producers(&mut self, mask: u64) {
            self.emptied |= mask;
        }
    }

    struct NoMerge;

    impl Merge for NoMerge {}

    static NO_MERGE: NoMerge = NoMerge;

    /// `join.rtfilter`'s export after `pipeline`, as `RtFilterLive::after_pipeline` asks it: `None` when nothing is
    /// exported there, else the filter's key range and its runs.
    fn exported(r: &Reader, pipeline: usize, run: &mut Run) -> Option<(i64, i64, Vec<(i64, i64)>)> {
        let f = r.after_build(pipeline, run, |read, o| m1::rt_filter(1, 64, &|| None, read, o.map(|o| (o, memledger::Kind::HashBuild))))?;
        f.map(|f| (f.lo, f.hi, f.runs.to_vec())) // alloc: Cold
    }

    fn reader(peer: usize) -> Reader {
        Reader { peer, slot: 0, read: super::super::raw::test_keys, words: memledger::ChargedVec::none() } // alloc: Cold
    }

    fn build_side(pipeline: usize) -> BuildSideLive {
        BuildSideLive { pipeline, merge: &NO_MERGE, slot: 0, read: Some(super::super::raw::test_rows), arg: MASK << 8 }
    }

    /// D4054 (R1): a build's keys are exported after the arm that finished its run, whichever arm the proto names. After
    /// `Q` alone the pair pass's keys are not in the slot yet: a filter armed then would skip the granules that hold them.
    #[test]
    fn a_builds_keys_are_exported_after_the_arm_that_finished_its_run() {
        for peer in [Q, P] {
            let (r, mut run) = (reader(peer), Run::new(false));
            run.append(&[5]);
            assert_eq!(exported(&r, Q, &mut run), None, "peer {peer}: no export after an arm that continues the run");
            run.append(&[9, 12]);
            assert_eq!(exported(&r, P, &mut run), Some((5, 12, vec![(5, 5), (9, 9), (12, 12)])), "peer {peer}: every arm's keys after the last arm"); // alloc: Cold
        }
    }

    /// D4054: a run that ended at its first arm (the pair pass has no row) exports after that arm; the stepped-past pair
    /// pass exports nothing again.
    #[test]
    fn a_run_that_ended_at_its_first_arm_exports_after_it() {
        for peer in [Q, P] {
            let (r, mut run) = (reader(peer), Run::new(true));
            run.append(&[3, 4]);
            assert_eq!(exported(&r, Q, &mut run), Some((3, 4, vec![(3, 4)])), "peer {peer}: the first arm finished the build"); // alloc: Cold
            assert_eq!(exported(&r, P, &mut run), None, "peer {peer}: the stepped-past arm");
        }
    }

    /// D4054 (R2): D276's reading waits for the arm that finished the build. A build whose rows all come from its pair
    /// pass is empty after `Q`: read there, the producers under the mask would be skipped over a build that has rows.
    #[test]
    fn an_empty_reading_waits_for_the_arm_that_finished_the_build() {
        for of in [Q, P] {
            let (b, mut run) = (build_side(of), Run::new(false));
            b.read_empty(Q, &mut run);
            assert_eq!(run.emptied, 0, "declared on {of}: the run's later arm may still append rows");
            run.append(&[7]);
            b.read_empty(P, &mut run);
            assert_eq!(run.emptied, 0, "declared on {of}: the finished build holds a row");
        }
    }

    /// An empty build is still read as empty: after the last arm of a run no arm of which appended a row, and after the
    /// first arm of a run that ended there.
    #[test]
    fn an_empty_build_skips_its_masked_producers_once_finished() {
        for of in [Q, P] {
            let (b, mut run) = (build_side(of), Run::new(false));
            b.read_empty(Q, &mut run);
            assert_eq!(run.emptied, 0, "declared on {of}: not after the arm that continues");
            b.read_empty(P, &mut run);
            assert_eq!(run.emptied, MASK, "declared on {of}: after the last arm");
            let mut ended = Run::new(true);
            b.read_empty(Q, &mut ended);
            assert_eq!(ended.emptied, MASK, "declared on {of}: after the arm the run ended at");
        }
    }

    /// A build of one pipeline is read after that pipeline, as before D4054.
    #[test]
    fn a_lone_build_is_read_after_its_own_pipeline() {
        let mut run = Run { lone: true, ..Run::new(false) };
        run.append(&[8]);
        assert_eq!(exported(&reader(Q), P, &mut run), None, "another pipeline");
        assert_eq!(exported(&reader(Q), Q, &mut run), Some((8, 8, vec![(8, 8)]))); // alloc: Cold
        let mut empty = Run { lone: true, ..Run::new(false) };
        build_side(Q).read_empty(P, &mut empty);
        assert_eq!(empty.emptied, 0, "another pipeline");
        build_side(Q).read_empty(Q, &mut empty);
        assert_eq!(empty.emptied, MASK);
    }
}

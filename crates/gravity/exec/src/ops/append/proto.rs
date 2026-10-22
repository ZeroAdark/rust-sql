//! M2's runtime protocols (cards/M2.md L3, exec/src/v4/m2.rs): `src.prune` (PostgreSQL's partition pruning once per
//! execution, into the arms' skip bits) and `append.arms`; the heap site admissions (ADMIT / BIND) are scan's sites_heap.rs.
//! Created by M2 (owner M2-HOSTS).
//!
//! `src.prune`'s contract with the lowerer (source/prune.rs `declare`, append.rs's arm marker):
//! - `arg` bits 0..15: the Append's `partPruneInfos` index; 16..31: its subplan count (`nsubplans`); 32..47: the subplan
//!   of the proto's first arm; 48..62: the proto's arm count; bit 63: the info has exec steps (`prune::has_exec_steps`).
//! - The proto's arms are subplans `first .. first + count`, run by pipelines `pipeline .. pipeline + count` (arms that are
//!   not contiguous pipelines take one proto per run).
//! - `words`: `ceil(count / 64)` words; bit `i` set = arm `i` pruned. Each arm's marker reads its bit at the open and runs
//!   empty; a pruned arm's pipeline also gets an empty claim source (`Live::claims_for`), so no worker walks it.
//! - Exec pruning reads the InitPlans' words (`GraphWords::inits`, filled by `run_inits` before this prepare) and the
//!   execution's extern parameters (`GraphWords::prune`'s handle, set by `eval_keys`).
//! - `peer`: a pull-form MergeAppend's reader (lowerv4 merge_pull.rs), which reads the skip words itself where its arms'
//!   pipelines run nothing: a pruning decided late or per rerun lays them at the reader's start too (prune_exec.rs,
//!   rerun/mod.rs); one decided here writes the binding's words, which every pipeline reads.
use std::borrow::Cow;

use gravity_parallel::{MorselRange, MorselSource, Unit};
use gravity_program::protos::{Proto, ProtoKind};
use memledger::ChargedVec;
use types_error::PgResult;
use types_portal::ParamListHandle;

use crate::ops::{Live, Prepare, Protocol};
use crate::glue;
use crate::ops::append::prune;
use crate::ops::append::prune::{ArmSet, ExecPrune, ExecWord, EXEC_WORDS};

pub const PROTOCOLS: &[&dyn Protocol] = &[&Prune, &Arms, &Volatile, &crate::ops::append::prune::NestPrune];

/// `src.prune`'s fields out of `arg` (see the module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PruneArg {
    pub index: u16,
    pub nsubplans: u16,
    pub first: u16,
    pub count: u16,
    pub exec: bool,
}

impl PruneArg {
    pub const fn arg(self) -> u64 {
        self.index as u64 | (self.nsubplans as u64) << 16 | (self.first as u64) << 32 | ((self.count & 0x7fff) as u64) << 48 | (self.exec as u64) << 63
    }
    pub const fn of(arg: u64) -> PruneArg {
        PruneArg { index: arg as u16, nsubplans: (arg >> 16) as u16, first: (arg >> 32) as u16, count: ((arg >> 48) & 0x7fff) as u16, exec: arg >> 63 != 0 }
    }
}

/// The skip words of the proto's arms from the surviving subplans `valid`: bit `i` = subplan `first + i` pruned.
pub fn skip_words(valid: &ArmSet, a: PruneArg, out: &mut [u64]) {
    out.iter_mut().for_each(|w| *w = 0);
    for i in 0..a.count as usize {
        if !prune::member(valid, a.first as usize + i) {
            out[i / 64] |= 1 << (i % 64);
        }
    }
}

/// D3017: a prune's skip words as its live keeps them between decisions, one word per 64 arms (`ceil(count / 64)`, the
/// proto's own word count), any arm count.
pub(crate) struct SkipWords(core::cell::RefCell<memledger::ChargedVec<u64>>);

impl SkipWords {
    pub(crate) fn new(words: memledger::ChargedVec<u64>) -> SkipWords {
        SkipWords(core::cell::RefCell::new(words))
    }
    /// `n` zero words: no arm skipped before the first decision.
    pub(crate) fn zeroed(n: usize, mut words: memledger::ChargedVec<u64>) -> SkipWords {
        words.with_vec(|v| { v.reserve_exact(n); v.resize(n, 0); }); // alloc: Exec charged words.owner()
        SkipWords::new(words)
    }
    /// The words of arms outside `valid` (`skip_words`), kept.
    pub(crate) fn decide(&self, valid: &ArmSet, a: PruneArg) {
        let mut w = self.0.borrow_mut();
        skip_words(valid, a, &mut w);
    }
    /// Whether arm `i` is skipped.
    pub(crate) fn skipped(&self, i: usize) -> bool {
        self.0.borrow().get(i / 64).is_some_and(|w| w >> (i % 64) & 1 != 0)
    }
    /// Word `k` (0 past the end).
    pub(crate) fn word(&self, k: usize) -> u64 {
        self.0.borrow().get(k).copied().unwrap_or(0)
    }
}

/// D3586: the arms the initial pruning removes this execution, decided before the runner sizes the claims and the sources
/// bind (runner plan.rs `RunnerPlan::prepare`, drive.rs `run_once`): each `src.prune` its initial steps alone decide (no
/// exec step, no rerun set: those decide later), as `Prune::prepare` decides it, which then takes the set from here (a
/// step's STABLE function runs once per execution). A removed arm's relation is neither sized nor opened, as PostgreSQL
/// never initializes a subplan its initial pruning removed: a generic EXECUTE over 2000 partitions that keeps one
/// opened, sized and bound all 2000.
#[derive(Default)]
pub struct Initial {
    /// Each decided proto's (first pipeline, arg) and its surviving subplans.
    sets: Vec<((u16, u64), ArmSet)>, // alloc: Exec
    /// A bit per pipeline the sets remove.
    pruned: Vec<u64>, // alloc: Exec
    /// D3663: each relation set its Append prunes at the executor start (`SiteDesc::set_prune`) and its surviving members.
    sites: Vec<(usize, ArmSet)>, // alloc: Exec
}

impl Initial {
    /// The initial-steps prunes of `program` (`a`'s, or an arm's swapped in) under `a`'s plan and parameters.
    pub fn decide(program: &gravity_program::Program, a: &crate::shapes::Admitted) -> PgResult<Initial> {
        Initial::decide_in(program, a.pstmt, Initial::params_of(a))
    }

    /// `a`'s execution parameters as its pruning reads them (`GraphWords::prune`'s handle; null without one).
    pub fn params_of(a: &crate::shapes::Admitted) -> ParamListHandle {
        a.graph.as_ref().and_then(|g| g.prune).map_or(ParamListHandle::NULL, |e| e.params)
    }

    /// `decide` of `program` (a statement's, an arm's or an InitPlan's) under plan `pstmt` and parameters `params`.
    pub fn decide_in(program: &gravity_program::Program, pstmt: Option<&'static types_nodes::plannodes::PlannedStmt<'static>>, params: ParamListHandle) -> PgResult<Initial> {
        let mut out = Initial::default();
        let protos = program.v4.as_deref().map_or(&[][..], |d| &d.protos[..]);
        let Some(pstmt) = pstmt else { return Ok(out) };
        for p in protos.iter().filter(|p| p.kind == ProtoKind::Prune) {
            let arg = PruneArg::of(p.arg);
            if arg.exec || crate::rerun::prunes(p, protos) || out.set_of(p).is_some() {
                continue;
            }
            let valid = prune::initial_valid(pstmt, arg.index as usize, params)?;
            for i in (0..arg.count).filter(|&i| !prune::member(&valid, (arg.first + i) as usize)) {
                let q = p.pipeline as usize + i as usize;
                if out.pruned.len() <= q / 64 {
                    out.pruned.resize(q / 64 + 1, 0); // alloc: Exec
                }
                out.pruned[q / 64] |= 1 << (q % 64);
            }
            out.sets.push(((p.pipeline, p.arg), valid)); // alloc: Exec
        }
        for (s, d) in program.sites.iter().enumerate() {
            if let Some(index) = d.set_prune {
                out.sites.push((s, prune::initial_valid(pstmt, index as usize, params)?)); // alloc: Exec
            }
        }
        Ok(out)
    }

    /// D3663: relation-set site `site`'s surviving members (by position in its set), when its Append prunes at the start.
    pub fn set_kept(&self, site: usize) -> Option<&ArmSet> {
        self.sites.iter().find(|(s, _)| *s == site).map(|(_, v)| v)
    }

    /// D3663: each relation-set site its Append prunes at the start, with its surviving members (runner drive.rs
    /// `file_pruned_sets` files their relations).
    pub fn set_sites(&self) -> impl Iterator<Item = (usize, &ArmSet)> {
        self.sites.iter().map(|(s, v)| (*s, v))
    }

    /// Whether the initial pruning removed pipeline `pipeline`'s arm.
    pub fn pruned(&self, pipeline: usize) -> bool {
        self.pruned.get(pipeline / 64).is_some_and(|w| w >> (pipeline % 64) & 1 != 0)
    }

    /// Proto `p`'s surviving subplans, when `decide` decided it.
    fn set_of(&self, p: &Proto) -> Option<&ArmSet> {
        self.sets.iter().find(|(k, _)| *k == (p.pipeline, p.arg)).map(|(_, v)| v)
    }

    /// The sites of `program` whose every driving pipeline the pruning removed: no pipeline this execution runs reads
    /// them (the runner files them for the sources' bind, hosts `Resolved::prune_site`).
    pub fn pruned_sites(&self, program: &gravity_program::Program) -> Vec<usize> {
        if self.pruned.is_empty() {
            return Vec::new(); // alloc: Exec
        }
        let n = program.sites.len();
        // per site: a pipeline drives it, and one this execution runs does
        let (mut driven, mut runs) = (vec![false; n], vec![false; n]); // alloc: Exec
        for (q, pipe) in program.pipelines.iter().enumerate() {
            let Some(s) = usize::try_from(pipe.driving_site).ok().filter(|&s| s < n) else { continue };
            driven[s] = true;
            runs[s] |= !self.pruned(q);
        }
        (0..n).filter(|&s| driven[s] && !runs[s]).collect() // alloc: Exec
    }
}

struct Prune;

impl Protocol for Prune {
    fn kind(&self) -> ProtoKind {
        ProtoKind::Prune
    }
    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        let a = PruneArg::of(p.arg);
        let (base, n) = (p.words.base as usize, p.words.n as usize);
        if n != (a.count as usize).div_ceil(64) || base + n > cx.words.len() {
            return Err(glue::internal("gravity: src.prune's skip words do not cover its arms"));
        }
        let Some(pstmt) = cx.admitted.pstmt else { return Err(glue::internal("gravity: src.prune without its plan")) };
        let g = cx.admitted.graph.as_ref();
        let params = g.and_then(|g| g.prune).map_or(ParamListHandle::NULL, |e| e.params);
        // M8 (L0-h): arms a rerun set re-runs decide at each rerun, over the owner's params (v4/m8_rerun.rs `prunes`); D2106:
        // the set decides initial steps only once, at its prepare, and skips the arms they remove whole
        if crate::rerun::prunes(p, cx.program.v4.as_ref().map_or(&[][..], |d| &d.protos[..])) {
            return Ok(None);
        }
        // M8 (L0-k): exec steps over an in-program InitPlan's param decide at the first arm's start (v4/m8_prune.rs)
        if let Some(late) = if a.exec { crate::ops::append::prune_exec::late(p, a, pstmt, params, cx)? } else { None } {
            return Ok(Some(late));
        }
        let valid: Cow<'_, ArmSet> = if a.exec {
            let mut words = [ExecWord { param: -1, value: 0, null: 1 }; EXEC_WORDS];
            let mut k = 0;
            for i in g.map_or(&[][..], |g| &g.inits[..]) {
                if k == EXEC_WORDS {
                    return Err(glue::internal("gravity: src.prune over more InitPlans than prune::EXEC_WORDS"));
                }
                if let (Some(&value), Some(&null)) = (cx.words.get(i.word), cx.nulls.get(i.word)) {
                    words[k] = ExecWord { param: i.param, value, null };
                    k += 1;
                }
            }
            let v = prune::exec_valid(pstmt, ExecPrune { index: a.index as u32, nsubplans: a.nsubplans, params }, &words[..k])?;
            fires::fire("append.exec_prune"); // shapecheck NEW-kernels-exec-121: a v4 program's exec-time arm skips (V4R-37 b)
            Cow::Owned(v)
        } else if let Some(v) = cx.initial.and_then(|i| i.set_of(p)) {
            // D3586: decided before the claims were sized (`Initial::decide`)
            Cow::Borrowed(v)
        } else {
            Cow::Owned(prune::initial_valid(pstmt, a.index as usize, params)?)
        };
        skip_words(&valid, a, &mut cx.words[base..base + n]);
        cx.nulls.get_mut(base..base + n).into_iter().flatten().for_each(|b| *b = 0);
        let mut pruned: ChargedVec<u16> = cx.charged_vec();
        pruned.with_vec(|v| v.extend((0..a.count).filter(|&i| !prune::member(&valid, (a.first + i) as usize)).map(|i| p.pipeline + i))); // alloc: Exec charged cx.owner
        if crate::inputs::knobs::seam::report_on() {
            // D509: the pruned arms, report-only (the e2e's witness that pruning fired)
            let _ = elog::ereport(types_error::LOG).errmsg(format!("gravity: src.prune pruned {} of {} arms (pipelines {:?})", pruned.len(), a.count, pruned.as_slice())).errhidestmt(true).errhidecontext(true).finish(types_error::ErrorLocation::new("gravity", 0, "prune")); // alloc: Cold
        }
        Ok((!pruned.is_empty()).then(|| Box::new(Pruned { pipelines: pruned }) as Box<dyn Live>)) // alloc: Exec
    }
}

/// This execution's pruned arm pipelines, ascending: each claims nothing.
struct Pruned {
    pipelines: ChargedVec<u16>,
}

impl Pruned {
    /// D3586: by binary search (the runner asks per pipeline: a scan of the list each time was quadratic in the arms).
    fn has(&self, pipeline: usize) -> bool {
        u16::try_from(pipeline).is_ok_and(|q| self.pipelines.binary_search(&q).is_ok())
    }
}

impl Live for Pruned {
    fn answers(&self) -> u16 {
        crate::ops::ask::CLAIMS | crate::ops::ask::PRUNED
    }

    fn claims_for(&self, pipeline: usize) -> Option<&dyn MorselSource> {
        self.has(pipeline).then_some(&NoClaims as &dyn MorselSource)
    }
    /// D2578: skipped whole (`Plan::skips_whole`), so a serial source (an index scan, which takes no claims) never walks
    /// a pruned arm nor calls its filter, as PostgreSQL never initializes an arm its pruning removed.
    fn pruned_whole(&self, pipeline: usize) -> bool {
        self.has(pipeline)
    }
}

/// A claim source with no morsels: a pruned arm's pipeline opens, claims nothing and ends.
pub struct NoClaims;

impl MorselSource for NoClaims {
    fn total(&self) -> u64 {
        0
    }
    fn unit(&self) -> Unit {
        Unit::Block
    }
    fn claim(&self) -> Option<MorselRange> {
        None
    }
    fn close(&self) {}
    fn remaining(&self) -> u64 {
        0
    }
    fn reset(&self) {}
}

/// `append.arms` (H05): the arms' pipelines each claim their own morsels, which the runner already gives every pipeline;
/// running them at once (and answering a fold per arm) is the runner's section scheduling, not built here: the protocol
/// checks the declaration and runs the arms one after another (served, not yet concurrent: a wall named in the lane report).
/// Arms that are Ineligible (a write streamed into its Append's arms, V4R-214) run on the leader in child order, whatever
/// the section scheduling V4R-39 builds: ModifyTable writes every row of arm i before arm i + 1 reads one.
struct Arms;

impl Protocol for Arms {
    fn kind(&self) -> ProtoKind {
        ProtoKind::Arms
    }
    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        if p.arg == 0 || p.pipeline as u64 + p.arg > cx.program.pipelines.len() as u64 {
            return Err(glue::internal("gravity: append.arms over pipelines the program does not have"));
        }
        Ok(None)
    }
}

/// `expr.volatile` (D439): a declaration the pull reads (driver pull.rs caps each call at the rows a fetch still owes);
/// nothing to prepare.
struct Volatile;

impl Protocol for Volatile {
    fn kind(&self) -> ProtoKind {
        ProtoKind::Volatile
    }
    fn prepare(&self, _p: &Proto, _cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The arg round-trips; the skip bits are the arms outside the surviving set, shifted to the proto's first subplan.
    #[test]
    fn prune_arg_and_skip_bits() {
        let a = PruneArg { index: 3, nsubplans: 70, first: 2, count: 67, exec: true };
        assert_eq!(PruneArg::of(a.arg()), a);
        let mut valid = vec![0u64; 2]; // alloc: Cold
        for s in [2usize, 5, 68] {
            valid[s / 64] |= 1 << (s % 64);
        }
        let valid = prune::ArmSet::Of(valid.into_boxed_slice());
        let mut w = [u64::MAX; 2];
        skip_words(&valid, a, &mut w);
        let kept: Vec<usize> = (0..67).filter(|i| w[i / 64] >> (i % 64) & 1 == 0).collect();
        assert_eq!(kept, [0, 3, 66], "subplans 2, 5, 68 are arms 0, 3, 66");
        // every arm pruned: every bit set; none: no bit (a surviving set of ALL)
        skip_words(&prune::ArmSet::Of(Box::new([])), a, &mut w); // alloc: Cold
        assert_eq!((w[0], w[1]), (u64::MAX, 0b111));
        skip_words(&prune::ALL, a, &mut w);
        assert_eq!(w, [0, 0]);
    }

    /// D3017: an Append past 256 subplans prunes: the kept set and the skip words reach every arm (5,000 here).
    #[test]
    fn skip_bits_past_256_arms() {
        let a = PruneArg { index: 0, nsubplans: 5001, first: 1, count: 5000, exec: true };
        let kept = [1usize, 257, 300, 5000];
        let mut words = vec![0u64; 5001usize.div_ceil(64)]; // alloc: Cold
        for s in kept {
            words[s / 64] |= 1 << (s % 64);
        }
        let valid = prune::ArmSet::Of(words.into_boxed_slice());
        let skip = SkipWords::zeroed(5000usize.div_ceil(64), memledger::ChargedVec::none());
        skip.decide(&valid, a);
        let alive: Vec<usize> = (0..5000).filter(|&i| !skip.skipped(i)).collect();
        assert_eq!(alive, [0, 256, 299, 4999], "subplans 1, 257, 300, 5000 are arms 0, 256, 299, 4999");
        assert_eq!(skip.word(78), !0u64 & !(1 << (4999 % 64)) & ((1 << (5000 % 64)) - 1), "the last word: arm 4999 alone kept of its 8");
        assert!(prune::member(&prune::ALL, 70_000) && !prune::member(&prune::ArmSet::Of(Box::new([])), 3)); // alloc: Cold
    }

    /// Does not apply: a pruned arm's claim source hands out nothing; an arm that survives keeps the runner's own.
    #[test]
    fn a_pruned_arm_claims_nothing_and_a_kept_arm_is_the_runners() {
        let mut pipelines: ChargedVec<u16> = ChargedVec::none();
        pipelines.with_vec(|v| v.extend([4, 6])); // alloc: Exec
        let live = Pruned { pipelines };
        let c = live.claims_for(4).expect("pruned");
        assert_eq!((c.total(), c.remaining()), (0, 0));
        assert!(c.claim().is_none());
        assert!(live.claims_for(5).is_none());
        // D2578: and the runner skips it whole (a serial source takes no claims)
        assert!(live.pruned_whole(4) && live.pruned_whole(6) && !live.pruned_whole(5));
    }
}

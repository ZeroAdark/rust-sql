//! `nest.rerun`'s Rerun protocol (cards/M8.md L5 nest.rerun, L2 RT-2; lowerv4 nest/rerun.rs): PG's rescan of a
//! parameterised inner, whose nodes with chgParam re-execute (ExecReScan), in a v4 program. A rerun set is the pipelines
//! decompose's `declare_rerun` found; lowerv4 declares one Rerun proto per member, all of a set sharing one handle word
//! (`arg` the set's index). The first of a set's protos lays the set's object into that word at every execution's
//! prepare; the members are deferred (`Live::deferred`): the producer order steps past them, and only the owner's re-open
//! runs them, by `m8_rerun(ctx, h)` after it wrote the param words: every call resets the members' slots in place and runs
//! them nested, in program order, through the runner's frame (`OnDemand::rerun_members`, RT-2e), as PG re-executes the
//! set at every outer row or SubPlan call whatever the params' values. A failed run's error is the
//! caller's host return (raised where the owner re-opens), and the set may run again at the next re-open, as PG's next
//! rescan would.
//!
//! The object: the `Head` (RERUN_MAGIC), then one word this crate keeps, the address of the set's runtime state
//! (`RerunRt`). No IR load reads it. It lives in the protocol's `Live`, made at every execution's prepare and dropped at
//! its end. Every pipeline whose code calls `m8_rerun` runs on the leader (lowerv4 D1035), whose frame the runner
//! publishes; a call without one is a Bug guard by name, never a value.
//!
//! L0-h (cards/M8.md; PG's ExecReScanAppend): a `src.prune` whose arms are the set's members and whose exec steps read the
//! owner's params is decided again at every rerun, before the members run: the params read from the caller's frame words
//! (the words site, as the owner wrote them), an in-program InitPlan's through its cell (L0-k's `read_param`), a graph
//! InitPlan's from its word; the skip words go to each arm at its start (`late_words`) and a pruned arm claims nothing.
//! D2106: a `src.prune` over the set's arms with initial steps only (an extern parameter's, a STABLE call's) is PG's
//! ExecInitAppend's, decided once at the prepare: its skip words hold for every run. m2.rs's Prune leaves either proto to
//! this live (`prunes`).
//! Created by M8-RERUN.
use core::cell::{Cell, RefCell};

use gravity_abi::Ctx;
use gravity_kernels::ops::shared::object::entries as abi;
use gravity_parallel::MorselSource;
use gravity_program::lifetime::{rerun_set_of, Lifetimes, RunLife};
use gravity_program::protos::{Proto, ProtoKind, NONE};
use memledger::{ChargedVec, Kind, Owner};
use types_error::PgResult;
use types_nodes::plannodes::PlannedStmt;

use crate::ops::append::proto::{skip_words, NoClaims, PruneArg, SkipWords};
use crate::ops::{Frame, Live, Prepare, Protocol};
use crate::glue;
use crate::ops::append::prune;
use crate::ops::append::prune::{ArmSet, ExecPrune, ExecWord, EXEC_WORDS};
pub mod epq;
mod raw;

pub struct Rerun;

/// The words of a set's object, a layout fact (not a capacity): the head (magic, op), then the state's address.
const WORDS: usize = core::mem::size_of::<abi::Head>() / 8 + 1;

/// One rerun set's runtime state for one execution.
struct RerunRt {
    /// The set's members in program order.
    members: ChargedVec<u16>,
    /// D1262: the members a run runs, in program order: every member but an InitPlan's, which the run re-arms (`inits`:
    /// its handle word and first param) and its next read runs, as PG's ExecReScanSetParamPlan re-decides an InitPlan lazily.
    run: ChargedVec<u16>,
    /// D1018 item 9 (b): the members a run after the first runs, `run` without those PG keeps at a rescan that changes no
    /// param (the program's lifetimes: `RunLife::FirstRun`, a nest inner's root Sort, top-k or hashed group, filled at the
    /// first re-open and rewound by its reader after); empty for a set kept whole, whose later re-opens run nothing.
    /// `filled`: a run of `run` ended well.
    again: ChargedVec<u16>,
    filled: Cell<bool>,
    /// D4120: an enclosing set's run changed a param a kept member's fill reads (`raw::stale_sets`): the next run runs `run`
    /// and fills the kept members again (`OnDemand::refill_members`), as PG's ExecReScanAgg rebuilds a kept table whose
    /// input's chgParam is set.
    stale: Cell<bool>,
    /// D4120: the handle words of the nested sets this set's run marks stale (the program's `SetRearms::refills`).
    refills: ChargedVec<usize>,
    inits: ChargedVec<(usize, i32)>,
    /// The LAZY hashed sets whose build is a member (their handle words): each run re-arms them (`rearm_sets`) and runs
    /// none of their members, so the next probe builds the set again over the new params (PG's ExecHashSubPlan after a
    /// rescan that changed them).
    sets: ChargedVec<usize>,
    /// setop_rec-0043: the CTEs whose pipeline is a member (their handle words): each run re-arms them (`rearm_ctes`) and
    /// runs none of their pulled sets, so their next pull starts the plan again (PG's ExecReScanCteScan).
    ctes: ChargedVec<usize>,
    /// setop_rec-0043: the handle words of the spools declared for those CTEs' pipelines (each store, a recursive union's
    /// two tables), whose count words the owner run resets: each run restarts their copies (ops/material/raw.rs).
    spools: ChargedVec<usize>,
    /// D4237: the WindowAggs whose pulled fill is a member (their handle words): each run re-arms them (ops/window/raw.rs
    /// `rearm_windows`) and runs none of their pulled sets, so their next pull fills the emptied spool from the fill's start
    /// (PG's ExecReScanWindowAgg).
    windows: ChargedVec<usize>,
    /// The owner: the pipeline whose code calls `m8_rerun` (the protos' `peer`; RT-2e's `Plan::rerun_owner`).
    owner: usize,
    /// The runner's frame while it runs this execution's program (`Live::frame`).
    frame: Cell<Option<Frame>>,
    /// A run in progress (a re-open inside the set's own run is a Bug by name).
    running: Cell<bool>,
    /// D2611: the owner's gated re-open re-armed the InitPlans ahead of its one-time gates (`OP_RERUN_REARM`): the run that
    /// follows a passing gate re-arms nothing (PG re-decides an InitPlan once per rescan) and clears it.
    armed: Cell<bool>,
    /// The runs so far this execution (EXPLAIN-free; the tests read it).
    runs: Cell<u64>,
    /// L0-h: the exec prunings over the set's arms, decided at each run, and the program's words site (the frame words).
    prunes: ChargedVec<RerunPrune>,
    words_site: usize,
}

/// L0-h: one `src.prune` over arms of the set, its exec steps over the owner's params (or D2106's initial steps only).
struct RerunPrune {
    pstmt: &'static PlannedStmt<'static>,
    prune: ExecPrune,
    arg: PruneArg,
    /// The first arm's pipeline: arm `i` is pipeline `pipeline + i`.
    pipeline: usize,
    /// A pull-form merge's reader (the proto's `peer`), whose start takes the skip words too: it reads the arms itself.
    reader: Option<usize>,
    base: usize,
    n: usize,
    /// The exec params in the steps' order: a graph InitPlan's word, an in-program Init's cell, else the owner's frame param.
    params: ChargedVec<(i32, Src)>,
    skip: SkipWords,
    /// D2106: initial steps only, decided at the prepare: `decide` keeps `skip`.
    fixed: bool,
    /// The exec pruning state kept across this execution's reruns (built at the first, rebuilt when spent).
    hold: RefCell<Option<prune::ExecHold>>,
}

impl RerunPrune {
    /// The subplans surviving under this rerun's parameters `words`, over the state the execution keeps (PostgreSQL's
    /// PartitionPruneState built once, ExecFindMatchingSubPlans at each rescan).
    fn valid(&self, words: &[ExecWord]) -> PgResult<ArmSet> {
        let mut hold = self.hold.borrow_mut();
        if hold.as_ref().is_none_or(|h| h.spent()) {
            *hold = None;
            *hold = Some(prune::ExecHold::new(self.pstmt, self.prune, words)?);
        }
        match hold.as_mut() {
            Some(h) => h.valid(words),
            None => Err(bad("m8_rerun: an exec pruning state not built")),
        }
    }
}

#[derive(Clone, Copy)]
enum Src {
    Known(u64, u8),
    Init(u64),
    Frame,
}

/// The protocol's `Live`: the object (the handle's target) and the state its trailing word names.
struct RerunLive {
    cell: Box<[u64]>,
    rt: Box<RerunRt>,
}

impl Live for RerunLive {
    fn answers(&self) -> u16 {
        crate::ops::ask::CLAIMS | crate::ops::ask::DEFERRED | crate::ops::ask::FRAME | crate::ops::ask::RERUN_OWNER | crate::ops::ask::LATE | crate::ops::ask::PRUNED
    }

    fn deferred(&self, pipeline: usize) -> bool {
        u16::try_from(pipeline).is_ok_and(|p| self.rt.members.binary_search(&p).is_ok())
    }

    fn frame(&self, f: Option<Frame>) {
        self.rt.frame.set(f);
    }

    fn rerun_owner(&self, pipeline: usize) -> Option<usize> {
        self.deferred(pipeline).then_some(self.rt.owner)
    }

    fn claims_for(&self, pipeline: usize) -> Option<&dyn MorselSource> {
        let (p, i) = self.rt.arm(pipeline)?;
        p.skip.skipped(i).then_some(&NoClaims as &dyn MorselSource)
    }

    /// D2105: an arm the set's prune skipped this run (its skip bit, decided by the run's `decide` before the members
    /// run) is never opened: PostgreSQL's ExecReScanAppend does not evaluate its runtime keys or the InitPlans they read.
    fn pruned_whole(&self, pipeline: usize) -> bool {
        self.rt.arm(pipeline).is_some_and(|(p, i)| p.skip.skipped(i))
    }

    fn late_words(&self, pipeline: usize, out: &mut gravity_parallel::LateWords) -> PgResult<()> {
        for p in self.rt.prunes.iter().filter(|p| p.reader == Some(pipeline) || pipeline.checked_sub(p.pipeline).is_some_and(|i| i < p.arg.count as usize)) {
            out.with_vec(|v| (0..p.n).for_each(|k| v.push((p.base + k, p.skip.word(k), 0)))); // alloc: Exec charged out
        }
        Ok(())
    }
}

impl RerunRt {
    /// The prune whose arm `pipeline` is, with the arm's index.
    fn arm(&self, pipeline: usize) -> Option<(&RerunPrune, usize)> {
        self.prunes.iter().find_map(|p| pipeline.checked_sub(p.pipeline).filter(|&i| i < p.arg.count as usize).map(|i| (p, i)))
    }
}

/// m2.rs's Prune: whether `p` (a src.prune proto) is decided by a rerun set's live (its first arm is a set's member).
pub(crate) fn prunes(p: &Proto, protos: &[Proto]) -> bool {
    protos.iter().any(|q| q.kind == ProtoKind::Rerun && q.pipeline == p.pipeline)
}

impl Protocol for Rerun {
    fn kind(&self) -> ProtoKind {
        ProtoKind::Rerun
    }

    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        let program = cx.program;
        match program.v4.as_deref() {
            Some(d) => prepare_in(p, &d.protos, &d.lifetimes, cx),
            None => prepare_in(p, &[], &Lifetimes::default(), cx),
        }
    }
}

/// `Rerun::prepare` over the program's protocols and run-again declarations (the frozen copy, or none).
fn prepare_in<A: gravity_program::Allocator + Clone>(p: &Proto, protos: &[Proto], lifetimes: &Lifetimes<A>, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
    let program = cx.program;
    let ps = prunes_of(p, protos, lifetimes, cx)?;
    let ws = program.sites.iter().position(|s| matches!(s.kind, gravity_program::SiteKind::Words { .. })).unwrap_or(usize::MAX);
    lay_owned(p, protos, lifetimes, program.pipelines.len(), cx.words, cx.nulls, cx.nparams, ps, ws, cx.owner)
}

/// M8 (RT-2e): the Rerun lives of `program` alone, one per set, laid as exec's prepare lays them (the crate rig's, as
/// m8_init.rs `prepare_bare`).
pub fn prepare_bare(program: &gravity_program::Program, words: &mut [u64], nulls: &mut [u8], nparams: &mut usize, out: &mut crate::ops::LiveList) -> PgResult<()> {
    match program.v4.as_deref() {
        Some(d) => bare_in(program, &d.protos, &d.lifetimes, words, nulls, nparams, out),
        None => bare_in(program, &[], &Lifetimes::default(), words, nulls, nparams, out),
    }
}

/// `prepare_bare` over the program's protocols and run-again declarations (the frozen copy, or none).
fn bare_in<A: gravity_program::Allocator + Clone>(program: &gravity_program::Program, protos: &[Proto], lifetimes: &Lifetimes<A>, words: &mut [u64], nulls: &mut [u8], nparams: &mut usize, out: &mut crate::ops::LiveList) -> PgResult<()> {
    for p in protos.iter().filter(|p| p.kind == ProtoKind::Rerun) {
        if protos.iter().any(|q| q.kind == ProtoKind::Prune && prunes(q, protos)) {
            return Err(glue::internal("gravity: the crate rig lays no src.prune over a rerun set's params (L0-h needs the plan)"));
        }
        let ws = program.sites.iter().position(|s| matches!(s.kind, gravity_program::SiteKind::Words { .. })).unwrap_or(usize::MAX);
        if let Some(l) = lay(p, protos, lifetimes, program.pipelines.len(), words, nulls, nparams, Vec::new(), ws)? { // alloc: Exec
            out.with_vec(|v| v.push(l)); // alloc: Exec charged out
        }
    }
    Ok(())
}

/// `lay_owned` with no owner (the crate rig's, and the tests'): every part a plain `Vec`.
#[allow(clippy::too_many_arguments)]
fn lay<A: gravity_program::Allocator + Clone>(p: &Proto, protos: &[Proto], lifetimes: &Lifetimes<A>, npipelines: usize, words: &mut [u64], nulls: &mut [u8], nparams: &mut usize, prunes: Vec<RerunPrune>, words_site: usize) -> PgResult<Option<Box<dyn Live>>> {
    let mut held: ChargedVec<RerunPrune> = ChargedVec::none();
    held.with_vec(|v| *v = prunes);
    lay_owned(p, protos, lifetimes, npipelines, words, nulls, nparams, held, words_site, None)
}

/// The set's object and state, its handle laid in its word, when `p` is its set's first proto (the others share it). What a
/// run runs and re-arms is the program's lifetimes' (lowerv4's lifetime home): its run (`Lifetimes::set_run`: the members
/// `Runner::Set` runs and those of a runner the set runs inside), in program order, those it runs again
/// (`RunLife::EveryRun`), and the set's re-arms (D1262 InitPlan cells, LAZY hashed sets, setop_rec-0043 CTEs and their
/// spools, D4237 windows whose pulled fill it holds), each of whose run-again members the lowering checked the set holds.
/// `ledger`: the prepare's owner (O6) the parts are charged to, when it is `Some`.
#[allow(clippy::too_many_arguments)]
fn lay_owned<A: gravity_program::Allocator + Clone>(p: &Proto, protos: &[Proto], lifetimes: &Lifetimes<A>, npipelines: usize, words: &mut [u64], nulls: &mut [u8], nparams: &mut usize, prunes: ChargedVec<RerunPrune>, words_site: usize, ledger: Option<(Owner, Kind)>) -> PgResult<Option<Box<dyn Live>>> {
    let (o, k) = ledger.map_or((None, Kind::Region), |(o, k)| (Some(o), k));
    let same = |q: &&Proto| q.kind == ProtoKind::Rerun && q.words.base == p.words.base;
    let mut set: ChargedVec<&Proto> = ChargedVec::new(o, k); // alloc: Exec charged o
    set.with_vec(|v| v.extend(protos.iter().filter(same))); // alloc: Exec charged o
    if !set.first().is_some_and(|f| core::ptr::eq(*f, p)) {
        return Ok(None);
    }
    let base = p.words.base as usize;
    let owner = p.peer as usize;
    let index = |q: &Proto| rerun_set_of(q.arg);
    if p.words.n != 1 || base >= words.len() || base >= nulls.len() || owner >= npipelines || set.iter().any(|q| index(q) != index(p) || q.peer != p.peer || q.pipeline as usize >= owner) {
        return Err(glue::internal("gravity: nest.rerun's declaration is not its set's (one handle word, one set index, one owner after every member)"));
    }
    let mut members: ChargedVec<u16> = ChargedVec::new(o, k); // alloc: Exec charged o
    members.with_vec(|v| v.extend(set.iter().map(|q| q.pipeline))); // alloc: Exec charged o
    members.sort_unstable();
    members.with_vec(|v| v.dedup());
    let at = index(p) as u16;
    let Some(re) = lifetimes.rearms(at) else {
        return Err(glue::internal("gravity: Bug: nest.rerun's set has no declaration in the program's lifetimes"));
    };
    let mut run: ChargedVec<u16> = ChargedVec::new(o, k); // alloc: Exec charged o
    run.with_vec(|v| v.extend(lifetimes.set_run(at).map(|m| m.pipeline))); // alloc: Exec charged o
    run.sort_unstable();
    if run.iter().any(|m| members.binary_search(m).is_err()) {
        return Err(glue::internal("gravity: Bug: the program's lifetimes run a pipeline again in nest.rerun's set that the set does not hold"));
    }
    let mut again: ChargedVec<u16> = ChargedVec::new(o, k); // alloc: Exec charged o
    again.with_vec(|v| v.extend(lifetimes.set_run(at).filter(|m| m.life == RunLife::EveryRun).map(|m| m.pipeline))); // alloc: Exec charged o
    again.sort_unstable();
    let words_of = |w: &[u16]| {
        let mut c: ChargedVec<usize> = ChargedVec::new(o, k); // alloc: Exec charged o
        c.with_vec(|v| v.extend(w.iter().map(|&x| x as usize))); // alloc: Exec charged o
        c
    };
    let mut inits: ChargedVec<(usize, i32)> = ChargedVec::new(o, k); // alloc: Exec charged o
    inits.with_vec(|v| v.extend(re.inits.iter().map(|&(w, first)| (w as usize, first)))); // alloc: Exec charged o
    let rt = Box::new(RerunRt { members, run, again, filled: Cell::new(false), stale: Cell::new(false), refills: words_of(&re.refills), inits, sets: words_of(&re.subhash), ctes: words_of(&re.ctes), spools: words_of(&re.spools), windows: words_of(&re.windows), owner, frame: Cell::new(None), running: Cell::new(false), armed: Cell::new(false), runs: Cell::new(0), prunes, words_site }); // alloc: Exec
    let cell = raw::object(&rt);
    words[base] = cell.as_ptr() as u64;
    nulls[base] = 0;
    *nparams = (*nparams).max(base + 1);
    Ok(Some(Box::new(RerunLive { cell, rt }))) // alloc: Exec
}

/// L0-h: the src.prune protos the program's lifetimes say `p`'s set decides again (their first arm one of its members; only
/// for the set's first proto), their exec
/// params sourced: a graph InitPlan's word, an in-program Init's cell (L0-k), else the owner's frame param. D2106: one with
/// initial steps only is decided here, once per execution over its extern parameters (PG's ExecInitAppend: the arms it
/// removes are never opened, `pruned_whole`), where its prepare raised XX000.
fn prunes_of<A: gravity_program::Allocator + Clone>(p: &Proto, protos: &[Proto], lifetimes: &Lifetimes<A>, cx: &mut Prepare<'_>) -> PgResult<ChargedVec<RerunPrune>> {
    let first = protos.iter().find(|q| q.kind == ProtoKind::Rerun && q.words.base == p.words.base);
    let mut out = cx.charged_vec();
    if !first.is_some_and(|f| core::ptr::eq(f, p)) {
        return Ok(out);
    }
    let decided = lifetimes.rearms(rerun_set_of(p.arg) as u16).map_or(&[][..], |r| &r.prunes[..]);
    for q in protos.iter().filter(|q| q.kind == ProtoKind::Prune && decided.contains(&q.words.base)) {
        let a = PruneArg::of(q.arg);
        let Some(pstmt) = cx.admitted.pstmt else { return Err(glue::internal("gravity: src.prune without its plan")) };
        let (base, n) = (q.words.base as usize, q.words.n as usize);
        if n != (a.count as usize).div_ceil(64) || base + n > cx.words.len() {
            return Err(glue::internal("gravity: nest.rerun's src.prune's skip words do not cover its arms (L0-h)"));
        }
        let g = cx.admitted.graph.as_ref();
        let params = g.and_then(|g| g.prune).map_or(types_portal::ParamListHandle::NULL, |e| e.params);
        if !a.exec {
            let mut skip: ChargedVec<u64> = cx.charged_vec();
            skip.with_vec(|v| { v.reserve_exact(n); v.resize(n, 0); }); // alloc: Exec charged cx.owner
            skip_words(&prune::initial_valid(pstmt, a.index as usize, params)?, a, &mut skip);
            cx.words[base..base + n].copy_from_slice(&skip);
            let prune = ExecPrune { index: a.index as u32, nsubplans: a.nsubplans, params };
            let skip = SkipWords::new(skip);
            out.with_vec(|v| v.push(RerunPrune { pstmt, prune, arg: a, pipeline: q.pipeline as usize, reader: (q.peer != NONE).then_some(q.peer as usize), base, n, params: ChargedVec::none(), skip, fixed: true, hold: RefCell::new(None) })); // alloc: Exec charged cx.owner
            continue;
        }
        let known = |x: i32| g.and_then(|g| g.inits.iter().find(|i| i.param == x)).and_then(|i| Some(Src::Known(*cx.words.get(i.word)?, *cx.nulls.get(i.word)?)));
        let init = |x: i32| protos.iter().find(|i| i.kind == ProtoKind::Init && gravity_kernels::ops::shared::object::entries::init_holds(i.arg, x)).map(|i| Src::Init(cx.words.get(i.words.base as usize).copied().unwrap_or(0)));
        let xs = exec_params(pstmt, a.index, cx);
        let mut ps: ChargedVec<(i32, Src)> = cx.charged_vec();
        ps.with_vec(|v| v.extend(xs.iter().map(|&x| (x, known(x).or_else(|| init(x)).unwrap_or(Src::Frame))))); // alloc: Exec charged cx.owner
        if ps.len() > EXEC_WORDS {
            return Err(glue::internal("gravity: src.prune over more params than prune::EXEC_WORDS"));
        }
        cx.words[base..base + n].iter_mut().for_each(|w| *w = 0);
        let prune = ExecPrune { index: a.index as u32, nsubplans: a.nsubplans, params };
        let skip = SkipWords::zeroed(n, cx.charged_vec());
        out.with_vec(|v| v.push(RerunPrune { pstmt, prune, arg: a, pipeline: q.pipeline as usize, reader: (q.peer != NONE).then_some(q.peer as usize), base, n, params: ps, skip, fixed: false, hold: RefCell::new(None) })); // alloc: Exec charged cx.owner
    }
    Ok(out)
}

/// The PARAM_EXEC ids the prune info's exec steps read (every partitioned rel's `execparamids`).
fn exec_params(pstmt: &'static PlannedStmt<'static>, index: u16, cx: &Prepare<'_>) -> ChargedVec<i32> {
    let mut out = cx.charged_vec();
    let Some(info) = pstmt.partPruneInfos.nth(index as usize).as_partition_prune_info() else { return out };
    for l in info.prune_infos.iter().filter_map(|l| l.as_list()) {
        for r in l.iter().filter_map(|r| r.as_partitioned_rel_prune_info()) {
            let mut x = r.execparamids.next_member(-1);
            while x >= 0 {
                if !out.contains(&x) {
                    out.with_vec(|v| v.push(x)); // alloc: Exec charged cx.owner
                }
                x = r.execparamids.next_member(x);
            }
        }
    }
    out
}

/// L0-h: every prune of the set decided over the params as the owner wrote them in `ctx`'s frame words (lowerv4
/// params.rs `word`: pair `2 * id`, value then flags; `SET` 1, `NULL` 2). D1262 (PG's ExecReScanSetParamPlan): first, each
/// InitPlan the set holds re-armed: its cell goes stale (exec initplan `rearm`) and its params' frame pairs are cleared, so
/// the next read runs it again through `m8_init_run`, never this run. D2611 (`ahead`, `OP_RERUN_REARM`): a gated nest
/// inner's re-open, before its one-time gates (PG's ExecReScan re-arms the node's InitPlans at the rescan, and the Result's
/// gate reads them at its first call after): only the re-arm, marked `armed`, so a gate reads this re-open's values also
/// when it fails and the set never runs; the run after a passing gate then re-arms nothing, and what the gate read is what
/// the members read.
///
/// `ctx` is the caller's context, bound for this call, as `frame_param` needs it.
fn decide(rt: &RerunRt, ctx: *mut Ctx, ahead: bool) -> PgResult<()> {
    if ahead && rt.running.get() {
        return Err(bad("m8_rerun_rearm: a rerun set re-armed inside its own run (D2611)"));
    }
    let rearm = ahead || !rt.armed.replace(false);
    if rearm && !rt.inits.is_empty() {
        raw::rearm_inits(ctx, rt.words_site, &rt.inits)?;
    }
    if ahead {
        rt.armed.set(true);
        return Ok(());
    }
    for p in rt.prunes.iter().filter(|p| !p.fixed) {
        let mut words = [ExecWord { param: -1, value: 0, null: 1 }; EXEC_WORDS];
        for (k, &(param, src)) in p.params.iter().enumerate() {
            let (value, null) = match src {
                Src::Known(v, n) => (v, n),
                Src::Init(cell) => crate::ops::subplan::initplan::init::read_param(cell, param)?,
                Src::Frame => raw::frame_param(ctx, rt.words_site, param)?,
            };
            words[k] = ExecWord { param, value, null };
        }
        let valid = p.valid(&words[..p.params.len()])?;
        p.skip.decide(&valid, p.arg);
    }
    Ok(())
}

/// Exec param `id`'s value and null (nonzero for NULL) from InitPlan cell `cell` (a handle word exec's
/// prepare laid), its members run first, nested, if this execution has not run them: L0-k's read, as a plain fn a host
/// object holds (the window object's lazy cells read an InitPlan's parameter at its first read: hosts window/lazy.rs).
pub fn init_param(cell: u64, id: i32) -> PgResult<(u64, u8)> {
    crate::ops::subplan::initplan::init::read_param(cell, id)
}

/// `OP_RERUN`: the set's prunes decided over the owner's params (L0-h), then the members' slots reset in place and the
/// members run now, nested, through the frame.
fn run(rt: &RerunRt, ctx: *mut Ctx) -> PgResult<()> {
    if rt.running.get() {
        return Err(bad("m8_rerun: a rerun set re-opened inside its own run"));
    }
    let Some(f) = rt.frame.get() else {
        return Err(bad("m8_rerun: no runner frame (bug: a re-open on a worker; D1035: the lowering pins every caller to the leader)"));
    };
    // `ctx` is the caller's (m8_rerun's first argument), bound while its call runs
    decide(rt, ctx, false)?;
    // a LAZY hashed set the set builds is re-armed, never run here (subplan/raw.rs)
    crate::ops::subplan::raw::rearm_sets(ctx, &rt.sets)?;
    // setop_rec-0043: a CTE the set re-arms, likewise: its next pull runs its plan again (ops/ctescan/raw.rs), into its
    // spools emptied (their counts the owner run's) with their copies restarted (ops/material/raw.rs)
    crate::ops::ctescan::raw::rearm_ctes(ctx, &rt.ctes)?;
    crate::ops::material::raw::restart_spools(ctx, &rt.spools)?;
    // D4237: a window whose pulled fill is a member, likewise: its spool emptied, its next pull runs the fill again
    crate::ops::window::raw::rearm_windows(ctx, &rt.windows)?;
    raw::stale_sets(ctx, &rt.refills)?;
    // D1018 item 9 (b): a kept member runs until a run fills it (PG keeps what its first rescan filled), never again
    // unless a param its fill reads changed since (D4120: `stale`)
    let refill = rt.stale.get();
    let first = !rt.filled.get() || refill;
    let members = if first { &rt.run } else { &rt.again };
    let r = match (members.is_empty(), refill) {
        (true, _) => Ok(()),
        (false, false) => run_set(f, &rt.running, members),
        (false, true) => refill_set(f, &rt.running, members),
    };
    rt.filled.set(rt.filled.get() || r.is_ok());
    rt.stale.set(refill && r.is_err());
    rt.runs.set(rt.runs.get() + 1);
    r.map_err(glue::pg_error)
}

/// D2034 `OP_RERUN_PULL` (PG's ExecRecursiveUnion pulling its term one row per call): a recursive union's term set whose
/// run is its body alone, pulled as a pulled set through the frame (`OnDemand::pull_members`) until the body's end took
/// the rows its want word allows (0) or ended (1). A generation's first pull (`first`) re-arms the set as `run` does
/// (prunes decided, LAZY sets, CTEs and their spools re-armed, nothing run) and the body's pull state with it
/// (`OnDemand::rearm_pulled`), so the body starts again over the new working table. A set with a kept member, or more
/// than its body to run, or a pull inside its own run, is a Bug by name.
fn pull(rt: &RerunRt, ctx: *mut Ctx, budget: u64, first: bool) -> PgResult<i64> {
    if rt.running.get() {
        return Err(bad("m8_rerun_pull: a rerun set pulled inside its own run (D2034)"));
    }
    let Some(f) = rt.frame.get() else {
        return Err(bad("m8_rerun_pull: no runner frame (bug: a pull on a worker; D1035: the lowering pins every caller to the leader)"));
    };
    if rt.run.len() != 1 || rt.again.as_slice() != rt.run.as_slice() {
        return Err(bad("m8_rerun_pull: a pulled term set runs more than its body, or keeps a member (D2034)"));
    }
    if first {
        decide(rt, ctx, false)?;
        crate::ops::subplan::raw::rearm_sets(ctx, &rt.sets)?;
        crate::ops::ctescan::raw::rearm_ctes(ctx, &rt.ctes)?;
        crate::ops::material::raw::restart_spools(ctx, &rt.spools)?;
        crate::ops::window::raw::rearm_windows(ctx, &rt.windows)?;
        raw::stale_sets(ctx, &rt.refills)?;
        raw::rearm_pulled(f, &rt.run).map_err(glue::pg_error)?;
        rt.runs.set(rt.runs.get() + 1);
    }
    rt.running.set(true);
    let r = raw::pull_members(f, &rt.run, budget.max(1));
    rt.running.set(false);
    rt.filled.set(rt.filled.get() || r.is_ok());
    match r.map_err(glue::pg_error)? {
        gravity_driver::Pulled::Done => Ok(1),
        gravity_driver::Pulled::Stopped => Ok(0),
    }
}

/// The rerun home's run half (interfaces.md §2.11 as corrected by proto2 §6.2): one owner re-open of a rerun set. The
/// members' slots reset in place and the members run nested, in program order, through the runner's frame
/// (`OnDemand::rerun_members`, RT-2e), `running` set meanwhile (a re-open inside the run is the caller's guard). Its
/// contract is every per-member, per-execution word re-armed before a member runs again, not only its slot.
pub fn run_set(frame: Frame, running: &Cell<bool>, members: &[u16]) -> gravity_driver::Result<()> {
    running.set(true);
    let r = raw::rerun_members(frame, members);
    running.set(false);
    r
}

/// D4120: `run_set` of a stale set, its kept members filled again too (`OnDemand::refill_members`).
fn refill_set(frame: Frame, running: &Cell<bool>, members: &[u16]) -> gravity_driver::Result<()> {
    running.set(true);
    let r = raw::refill_members(frame, members);
    running.set(false);
    r
}

#[cold]
#[inline(never)]
fn bad(what: &str) -> Box<types_error::PgError> {
    glue::internal(&format!("gravity: {what}")) // alloc: Cold
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use core::ptr::NonNull;

    use gravity_abi::status::ERROR;
    use gravity_program::lifetime::{rerun_arg, RunMember, Runner, SetRearms, NO_PROBE};

    use super::*;

    fn cv<T: Copy>(s: &[T]) -> ChargedVec<T> {
        let mut c: ChargedVec<T> = ChargedVec::none();
        c.with_vec(|v| v.extend_from_slice(s));
        c
    }

    /// A frame that records the member runs (`refilled`: the indexes of those that filled kept members again) and answers
    /// with `then`.
    struct Rec {
        runs: Vec<Vec<u16>>,
        refilled: Vec<usize>,
        then: Option<gravity_driver::Error>,
    }

    impl gravity_driver::OnDemand for Rec {
        fn run_members(&mut self, _members: &[u16]) -> gravity_driver::Result<()> {
            panic!("a rerun set runs through rerun_members, never run_members (its slots are live)")
        }

        fn rerun_members(&mut self, members: &[u16]) -> gravity_driver::Result<()> {
            self.runs.push(members.to_vec()); // alloc: Cold
            self.then.take().map_or(Ok(()), Err)
        }

        fn refill_members(&mut self, members: &[u16]) -> gravity_driver::Result<()> {
            self.refilled.push(self.runs.len()); // alloc: Cold
            self.rerun_members(members)
        }
    }

    fn proto(pipeline: u16, h: u16, set: u64) -> Proto {
        Proto::new(ProtoKind::Rerun, pipeline).peer(5).words(h, 1).arg(set)
    }

    fn member(pipeline: u16, runner: Runner, life: RunLife) -> RunMember {
        RunMember { pipeline, runner, life, lazy_probe: NO_PROBE }
    }

    /// The program's lifetimes as the lowering declares sets with nothing to re-arm: each member its proto's set's, kept
    /// for the execution when its proto carries the kept flag.
    fn plain(protos: &[Proto]) -> Lifetimes {
        let life = |q: &Proto| if q.arg == rerun_set_of(q.arg) { RunLife::EveryRun } else { RunLife::FirstRun };
        let ms = protos.iter().map(|q| member(q.pipeline, Runner::Set(rerun_set_of(q.arg) as u16), life(q))).collect(); // alloc: Cold
        let n = protos.iter().map(|q| rerun_set_of(q.arg) as usize + 1).max().unwrap_or(0);
        Lifetimes::new(ms).expect("one runner per member").with_rearms(vec![SetRearms::default(); n]) // alloc: Cold
    }

    /// The lives `lay` makes over `protos` (one per set) under `lifetimes`, with the words they laid.
    fn lives_in(protos: &[Proto], lifetimes: &Lifetimes, n: usize) -> (Vec<Box<dyn Live>>, Vec<u64>) {
        let (mut words, mut nulls, mut nparams) = (vec![0u64; 8], vec![1u8; 8], 0usize); // alloc: Cold
        let mut out = Vec::new(); // alloc: Cold
        for p in protos {
            out.extend(lay(p, protos, lifetimes, n, &mut words, &mut nulls, &mut nparams, Vec::new(), usize::MAX).expect("a well-formed set")); // alloc: Cold
        }
        (out, words)
    }

    fn lives(protos: &[Proto], n: usize) -> (Vec<Box<dyn Live>>, Vec<u64>) {
        lives_in(protos, &plain(protos), n)
    }

    /// Set 0 = pipelines 1, 2 (an InitPlan's, its cell's handle word 6) and 3 (the set's own) on word 2.
    fn with_init() -> Lifetimes {
        let ms = vec![member(1, Runner::Init(6), RunLife::EveryRun), member(2, Runner::Init(6), RunLife::EveryRun), member(3, Runner::Set(0), RunLife::EveryRun)]; // alloc: Cold
        Lifetimes::new(ms).unwrap().with_rearms(vec![SetRearms { inits: [(6, 0)].into(), ..SetRearms::default() }]) // alloc: Cold
    }

    fn frame(r: &mut Rec) -> Frame {
        let p: *mut (dyn gravity_driver::OnDemand + '_) = r;
        // SAFETY: the test keeps `r` alive past every use of the frame.
        NonNull::new(unsafe { core::mem::transmute::<*mut (dyn gravity_driver::OnDemand + '_), *mut (dyn gravity_driver::OnDemand + 'static)>(p) }).unwrap()
    }

    fn message() -> String {
        gravity_fn::take_error().map(|e| e.message().to_owned()).unwrap_or_default()
    }

    #[test]
    fn a_set_is_one_object_over_its_members_and_every_call_reruns_them() {
        // set 0 = pipelines 3, 1 (declared out of order) on word 2; set 1 = pipeline 4 on word 5
        let ps = [proto(3, 2, 0), proto(1, 2, 0), proto(4, 5, 1)];
        let (ls, words) = lives(&ps, 6);
        assert_eq!(ls.len(), 2, "one live per set");
        assert!(ls[0].deferred(1) && ls[0].deferred(3) && !ls[0].deferred(2) && !ls[0].deferred(4));
        assert!(ls[1].deferred(4) && !ls[1].deferred(3));
        assert_eq!((ls[0].rerun_owner(3), ls[0].rerun_owner(2), ls[1].rerun_owner(4)), (Some(5), None, Some(5)));
        let mut r = Rec { runs: Vec::new(), refilled: Vec::new(), then: None }; // alloc: Cold
        ls.iter().for_each(|l| l.frame(Some(frame(&mut r))));
        let ctx: *mut Ctx = core::ptr::null_mut();
        for _ in 0..3 {
            assert_eq!(unsafe { abi::m8_rerun(ctx as u64, words[2]) }, 0);
        }
        assert_eq!(unsafe { abi::m8_rerun(ctx as u64, words[5]) }, 0);
        ls.iter().for_each(|l| l.frame(None));
        assert_eq!(r.runs, [vec![1u16, 3], vec![1, 3], vec![1, 3], vec![4]], "PG's chgParam: every re-open reruns, in program order"); // alloc: Cold
    }

    #[test]
    fn a_failed_run_raises_at_the_caller_and_the_next_reopen_runs_again() {
        let (ls, words) = lives(&[proto(0, 1, 0)], 6);
        // (an error the glue maps without the interrupt seam, which a unit test does not install)
        let mut r = Rec { runs: Vec::new(), refilled: Vec::new(), then: Some(gravity_driver::Error::Host("a member failed".into())) }; // alloc: Cold
        ls[0].frame(Some(frame(&mut r)));
        let ctx: *mut Ctx = core::ptr::null_mut();
        assert_eq!(unsafe { abi::m8_rerun(ctx as u64, words[1]) }, ERROR);
        assert!(message().contains("a member failed"), "the run's error is stashed for the caller's raise");
        assert_eq!(unsafe { abi::m8_rerun(ctx as u64, words[1]) }, 0);
        assert_eq!(r.runs.len(), 2);
    }

    #[test]
    fn d1262_an_initplans_members_are_re_armed_never_run_by_the_set() {
        // set 0 = pipelines 1, 2, 3 on word 2; 1 and 2 are an InitPlan's (its handle word 6), 3 is run by the set
        let ps = [proto(1, 2, 0), proto(2, 2, 0), proto(3, 2, 0)];
        let (mut words, mut nulls, mut n) = (vec![0u64; 8], vec![1u8; 8], 0usize); // alloc: Cold
        let l = lay(&ps[0], &ps, &with_init(), 6, &mut words, &mut nulls, &mut n, Vec::new(), usize::MAX).expect("a set holding an InitPlan").expect("the set's live"); // alloc: Cold
        assert!(l.deferred(1) && l.deferred(2) && l.deferred(3), "every member stays the set's (its slots kept)");
        let mut r = Rec { runs: Vec::new(), refilled: Vec::new(), then: None }; // alloc: Cold
        l.frame(Some(frame(&mut r)));
        let ctx: *mut Ctx = core::ptr::null_mut();
        assert_eq!(unsafe { abi::m8_rerun(ctx as u64, words[2]) }, ERROR, "no frame words to clear the InitPlan's pairs in");
        assert!(message().contains("no frame words (D1262)"));
        assert!(r.runs.is_empty());
        // a pipeline the lifetimes give the set's run but the set does not hold is a declaration the lowering never makes
        let outside = Lifetimes::new(vec![member(0, Runner::Set(0), RunLife::EveryRun), member(1, Runner::Init(6), RunLife::EveryRun)]).unwrap().with_rearms(vec![SetRearms::default()]); // alloc: Cold
        assert!(lay(&ps[0], &ps, &outside, 6, &mut words, &mut nulls, &mut n, Vec::new(), usize::MAX).is_err()); // alloc: Cold
        let rt = Box::new(RerunRt { members: cv(&[1u16, 2, 3]), run: cv(&[3u16]), again: cv(&[3u16]), filled: Cell::new(false), stale: Cell::new(false), refills: ChargedVec::none(), inits: ChargedVec::none(), sets: ChargedVec::none(), ctes: ChargedVec::none(), spools: ChargedVec::none(), windows: ChargedVec::none(), owner: 5, frame: Cell::new(Some(frame(&mut r))), running: Cell::new(false), armed: Cell::new(false), runs: Cell::new(0), prunes: ChargedVec::none(), words_site: usize::MAX }); // alloc: Cold
        run(&rt, ctx).expect("a set with nothing to re-arm");
        assert_eq!(r.runs, [vec![3u16]], "the set runs only its own members"); // alloc: Cold
    }

    #[test]
    fn d2611_a_gated_reopen_re_arms_ahead_and_the_run_after_re_arms_nothing() {
        // set 0 = pipelines 1, 2 (an InitPlan's, its handle word 6) and 3 on word 2: the re-arm ahead fails by D1262's name
        // with no frame words to clear, as the run's own re-arm does, and marks nothing
        let ps = [proto(1, 2, 0), proto(2, 2, 0), proto(3, 2, 0)];
        let (mut words, mut nulls, mut n) = (vec![0u64; 8], vec![1u8; 8], 0usize); // alloc: Cold
        let l = lay(&ps[0], &ps, &with_init(), 6, &mut words, &mut nulls, &mut n, Vec::new(), usize::MAX).expect("a set holding an InitPlan").expect("the set's live"); // alloc: Cold
        let mut r = Rec { runs: Vec::new(), refilled: Vec::new(), then: None }; // alloc: Cold
        l.frame(Some(frame(&mut r)));
        let ctx: *mut Ctx = core::ptr::null_mut();
        assert_eq!(unsafe { gravity_kernels::contract::entry1::<gravity_kernels::ops::shared::object::raw::RerunRearm>(ctx as u64, words[2]) } as i64, ERROR);
        assert!(message().contains("no frame words (D1262)"));
        assert!(r.runs.is_empty(), "the re-arm ahead runs no member");
        // a set with nothing to re-arm: the re-arm ahead marks the coming run, which clears the mark (a failed gate leaves it
        // for the next re-open's re-arm, which marks it again)
        let rt = Box::new(RerunRt { members: cv(&[3u16]), run: cv(&[3u16]), again: cv(&[3u16]), filled: Cell::new(false), stale: Cell::new(false), refills: ChargedVec::none(), inits: ChargedVec::none(), sets: ChargedVec::none(), ctes: ChargedVec::none(), spools: ChargedVec::none(), windows: ChargedVec::none(), owner: 5, frame: Cell::new(Some(frame(&mut r))), running: Cell::new(false), armed: Cell::new(false), runs: Cell::new(0), prunes: ChargedVec::none(), words_site: usize::MAX }); // alloc: Cold
        decide(&rt, ctx, true).expect("nothing to re-arm");
        decide(&rt, ctx, true).expect("a failed gate's re-open, then the next");
        assert!(rt.armed.get() && r.runs.is_empty());
        run(&rt, ctx).expect("the run after a passing gate");
        assert!(!rt.armed.get(), "the run consumed the mark");
        assert_eq!(r.runs, [vec![3u16]]); // alloc: Cold
        rt.running.set(true);
        assert!(decide(&rt, ctx, true).is_err(), "a re-arm inside the set's own run is a Bug by name");
    }

    #[test]
    fn a_lazy_hashed_sets_build_members_stay_the_sets_and_lie_inside_it() {
        // set 0 = pipelines 1, 2, 3 on word 2; 1 builds a LAZY hashed set (its handle word 7), re-armed by each run
        let ps = [proto(1, 2, 0), proto(2, 2, 0), proto(3, 2, 0)];
        let (mut words, mut nulls, mut n) = (vec![0u64; 8], vec![1u8; 8], 0usize); // alloc: Cold
        let ms = || vec![member(1, Runner::SubHash(7), RunLife::EveryRun), member(2, Runner::Set(0), RunLife::EveryRun), member(3, Runner::Set(0), RunLife::EveryRun)]; // alloc: Cold
        let lazy = Lifetimes::new(ms()).unwrap().with_rearms(vec![SetRearms { subhash: [7].into(), ..SetRearms::default() }]); // alloc: Cold
        let l = lay(&ps[0], &ps, &lazy, 6, &mut words, &mut nulls, &mut n, Vec::new(), usize::MAX).expect("a set holding a LAZY build").expect("the set's live"); // alloc: Cold
        assert!(l.deferred(1) && l.deferred(2) && l.deferred(3), "every member stays the set's (the build runs at its next probe)");
        // a set the program's lifetimes do not declare is a Bug by name, before anything is laid
        let undeclared = Lifetimes::new(ms()).unwrap();
        assert!(lay(&ps[0], &ps, &undeclared, 6, &mut words, &mut nulls, &mut n, Vec::new(), usize::MAX).is_err()); // alloc: Cold
    }

    #[test]
    fn d1018_a_kept_member_runs_until_a_run_fills_it_and_never_again() {
        // set 0 = pipelines 1 (kept: a root Sort's fill) and 2 (re-run) on word 2; set 1 = pipeline 4, kept whole, on word 5
        let kept = |set: u64| rerun_arg(set as usize, true);
        let ps = [proto(1, 2, kept(0)), proto(2, 2, 0), proto(4, 5, kept(1))];
        let (ls, words) = lives(&ps, 6);
        assert_eq!(ls.len(), 2, "the kept bit is no part of the set's index");
        assert!(ls[0].deferred(1) && ls[0].deferred(2) && ls[1].deferred(4), "a kept member stays deferred: its first run is the owner's");
        let mut r = Rec { runs: Vec::new(), refilled: Vec::new(), then: Some(gravity_driver::Error::Host("the fill failed".into())) }; // alloc: Cold
        ls.iter().for_each(|l| l.frame(Some(frame(&mut r))));
        let ctx: *mut Ctx = core::ptr::null_mut();
        assert_eq!(unsafe { abi::m8_rerun(ctx as u64, words[2]) }, ERROR);
        let _ = message();
        for _ in 0..3 {
            assert_eq!(unsafe { abi::m8_rerun(ctx as u64, words[2]) }, 0);
            assert_eq!(unsafe { abi::m8_rerun(ctx as u64, words[5]) }, 0);
        }
        ls.iter().for_each(|l| l.frame(None));
        assert_eq!(r.runs, [vec![1u16, 2], vec![1, 2], vec![4], vec![2], vec![2]], "a failed run fills nothing; then each set's kept members ran once"); // alloc: Cold
    }

    #[test]
    fn d4120_an_enclosing_run_marks_a_nested_sets_kept_members_stale_and_its_next_run_fills_them_again() {
        // set 0 = pipelines 1 (kept: a hashed fill reading set 1's param) and 2 on word 2; set 1 = pipeline 4 on word 5, its
        // runs marking set 0 stale (its refills)
        let ps = [proto(1, 2, rerun_arg(0, true)), proto(2, 2, 0), proto(4, 5, 1)];
        let ms = vec![member(1, Runner::Set(0), RunLife::FirstRun), member(2, Runner::Set(0), RunLife::EveryRun), member(4, Runner::Set(1), RunLife::EveryRun)]; // alloc: Cold
        let l = Lifetimes::new(ms).unwrap().with_rearms(vec![SetRearms::default(), SetRearms { refills: [2].into(), ..SetRearms::default() }]); // alloc: Cold
        let (ls, words) = lives_in(&ps, &l, 6);
        let mut r = Rec { runs: Vec::new(), refilled: Vec::new(), then: None }; // alloc: Cold
        ls.iter().for_each(|l| l.frame(Some(frame(&mut r))));
        // a context whose params are the laid handle words
        let mut hdr = [0u64; gravity_abi::ctx::hdr::SIZE / 8];
        hdr[gravity_abi::ctx::hdr::PARAMS / 8] = words.as_ptr() as u64;
        let ctx = hdr.as_mut_ptr().cast::<Ctx>();
        let (inner, outer) = (words[2], words[5]);
        assert_eq!(unsafe { abi::m8_rerun(ctx as u64, outer) }, 0, "an unfilled set is no stale one");
        assert_eq!(unsafe { abi::m8_rerun(ctx as u64, inner) }, 0);
        assert_eq!(unsafe { abi::m8_rerun(ctx as u64, inner) }, 0);
        assert_eq!(unsafe { abi::m8_rerun(ctx as u64, outer) }, 0);
        r.then = Some(gravity_driver::Error::Host("the refill failed".into()));
        assert_eq!(unsafe { abi::m8_rerun(ctx as u64, inner) }, ERROR);
        let _ = message();
        assert_eq!(unsafe { abi::m8_rerun(ctx as u64, inner) }, 0, "a failed refill stays stale");
        assert_eq!(unsafe { abi::m8_rerun(ctx as u64, inner) }, 0);
        ls.iter().for_each(|l| l.frame(None));
        assert_eq!(r.runs, [vec![4u16], vec![1, 2], vec![2], vec![4], vec![1, 2], vec![1, 2], vec![2]]); // alloc: Cold
        assert_eq!(r.refilled, [4, 5], "the runs after the enclosing run fill the kept member again, then keep it"); // alloc: Cold
        // a refill word that names no rerun set is a Bug by name
        let ms = vec![member(4, Runner::Set(0), RunLife::EveryRun)]; // alloc: Cold
        let odd = Lifetimes::new(ms).unwrap().with_rearms(vec![SetRearms { refills: [6].into(), ..SetRearms::default() }]); // alloc: Cold
        let (ls, words) = lives_in(&[proto(4, 5, 0)], &odd, 6);
        ls[0].frame(Some(frame(&mut r)));
        hdr[gravity_abi::ctx::hdr::PARAMS / 8] = words.as_ptr() as u64;
        assert_eq!(unsafe { abi::m8_rerun(ctx as u64, words[5]) }, ERROR);
        assert!(message().contains("not its rerun set (D4120)"));
        ls[0].frame(None);
    }

    /// A frame that records the pulled set's re-arms and pulls and answers each pull from `answers`.
    struct Pulls {
        log: Vec<String>,
        answers: Vec<gravity_driver::Pulled>,
    }

    impl gravity_driver::OnDemand for Pulls {
        fn run_members(&mut self, _members: &[u16]) -> gravity_driver::Result<()> {
            panic!("a pulled term set never runs whole")
        }

        fn rearm_pulled(&mut self, members: &[u16]) -> gravity_driver::Result<()> {
            self.log.push(format!("rearm {members:?}")); // alloc: Cold
            Ok(())
        }

        fn pull_members(&mut self, members: &[u16], budget: u64) -> gravity_driver::Result<gravity_driver::Pulled> {
            self.log.push(format!("pull {members:?} {budget}")); // alloc: Cold
            Ok(self.answers.remove(0))
        }
    }

    fn rt_over(run: &[u16], again: &[u16], f: Frame) -> Box<RerunRt> {
        Box::new(RerunRt { members: cv(run), run: cv(run), again: cv(again), filled: Cell::new(false), stale: Cell::new(false), refills: ChargedVec::none(), inits: ChargedVec::none(), sets: ChargedVec::none(), ctes: ChargedVec::none(), spools: ChargedVec::none(), windows: ChargedVec::none(), owner: 5, frame: Cell::new(Some(f)), running: Cell::new(false), armed: Cell::new(false), runs: Cell::new(0), prunes: ChargedVec::none(), words_site: usize::MAX }) // alloc: Cold
    }

    #[test]
    fn d2034_a_generations_first_pull_re_arms_the_body_and_each_pull_takes_its_budget() {
        let mut p = Pulls { log: Vec::new(), answers: vec![gravity_driver::Pulled::Stopped, gravity_driver::Pulled::Done, gravity_driver::Pulled::Done] }; // alloc: Cold
        let pp: *mut (dyn gravity_driver::OnDemand + '_) = &mut p;
        // SAFETY: the test keeps `p` alive past every use of the frame.
        let f = NonNull::new(unsafe { core::mem::transmute::<*mut (dyn gravity_driver::OnDemand + '_), *mut (dyn gravity_driver::OnDemand + 'static)>(pp) }).unwrap();
        let ctx: *mut Ctx = core::ptr::null_mut();
        let rt = rt_over(&[3], &[3], f);
        assert_eq!(pull(&rt, ctx, 1, true).expect("the first pull"), 0, "stopped after its budget");
        assert_eq!(pull(&rt, ctx, 2, false).expect("the next pull"), 1, "the body ended");
        assert_eq!(pull(&rt, ctx, 0, true).expect("the next generation's first pull"), 1);
        assert_eq!(rt.runs.get(), 2, "one run per generation");
        // a set that runs more than its body, or keeps a member, is a Bug by name before anything is pulled
        let two = rt_over(&[2, 3], &[2, 3], f);
        assert!(pull(&two, ctx, 1, true).is_err_and(|e| e.message().contains("m8_rerun_pull: a pulled term set runs more than its body")));
        let kept = rt_over(&[3], &[], f);
        assert!(pull(&kept, ctx, 1, true).is_err());
        assert_eq!(p.log, ["rearm [3]", "pull [3] 1", "pull [3] 2", "rearm [3]", "pull [3] 1"], "a zero budget pulls one row");
    }

    /// What a run runs and runs again is the program's lifetimes', not the member protos' bits.
    #[test]
    fn the_lifetimes_say_which_members_a_later_run_runs() {
        let ps = [proto(1, 2, 0), proto(2, 2, 0)];
        let l = Lifetimes::new(vec![member(1, Runner::Set(0), RunLife::FirstRun), member(2, Runner::Set(0), RunLife::EveryRun)]).unwrap().with_rearms(vec![SetRearms::default()]); // alloc: Cold
        let (ls, words) = lives_in(&ps, &l, 6);
        let mut r = Rec { runs: Vec::new(), refilled: Vec::new(), then: None }; // alloc: Cold
        ls[0].frame(Some(frame(&mut r)));
        let ctx: *mut Ctx = core::ptr::null_mut();
        for _ in 0..3 {
            assert_eq!(unsafe { abi::m8_rerun(ctx as u64, words[2]) }, 0);
        }
        ls[0].frame(None);
        assert_eq!(r.runs, [vec![1u16, 2], vec![2], vec![2]], "the member declared FirstRun ran once"); // alloc: Cold
    }

    /// A set inside an InitPlan's closure (a correlated SubPlan under the InitPlan's scan) runs the members it holds that
    /// are declared the cell's, with the lives the set gives them.
    #[test]
    fn a_set_inside_a_cells_closure_runs_its_members_declared_the_cells() {
        let ps = [proto(2, 2, 0), proto(1, 2, rerun_arg(0, true))];
        let nested = SetRearms { nested: [(1, RunLife::FirstRun), (2, RunLife::EveryRun)].into(), ..SetRearms::default() }; // alloc: Cold
        let l = Lifetimes::new(vec![member(0, Runner::Init(6), RunLife::EveryRun), member(1, Runner::Init(6), RunLife::EveryRun), member(2, Runner::Init(6), RunLife::EveryRun)]).unwrap().with_rearms(vec![nested]); // alloc: Cold
        let (ls, words) = lives_in(&ps, &l, 6);
        let mut r = Rec { runs: Vec::new(), refilled: Vec::new(), then: None }; // alloc: Cold
        ls[0].frame(Some(frame(&mut r)));
        let ctx: *mut Ctx = core::ptr::null_mut();
        for _ in 0..3 {
            assert_eq!(unsafe { abi::m8_rerun(ctx as u64, words[2]) }, 0);
        }
        ls[0].frame(None);
        assert_eq!(r.runs, [vec![1u16, 2], vec![2], vec![2]], "in program order; the kept one ran once"); // alloc: Cold
    }

    #[test]
    fn a_call_without_a_frame_is_an_error_by_name() {
        let (_ls, words) = lives(&[proto(0, 1, 0)], 6);
        let ctx: *mut Ctx = core::ptr::null_mut();
        assert_eq!(unsafe { abi::m8_rerun(ctx as u64, words[1]) }, ERROR);
        assert!(message().starts_with("gravity: m8_rerun: no runner frame"));
    }

    #[test]
    fn a_malformed_declaration_is_an_error_at_the_prepare() {
        let (mut words, mut nulls, mut n) = (vec![0u64; 4], vec![0u8; 4], 0usize); // alloc: Cold
        // two set indexes on one handle word
        let ps = [proto(0, 1, 0), proto(1, 1, 1)];
        assert!(lay(&ps[0], &ps, &plain(&ps), 6, &mut words, &mut nulls, &mut n, Vec::new(), usize::MAX).is_err()); // alloc: Cold
        // an owner past the program's pipelines, and a member at or after its owner
        let ps = [proto(0, 1, 0)];
        assert!(lay(&ps[0], &ps, &plain(&ps), 5, &mut words, &mut nulls, &mut n, Vec::new(), usize::MAX).is_err()); // alloc: Cold
        let ps = [proto(5, 1, 0)];
        assert!(lay(&ps[0], &ps, &plain(&ps), 6, &mut words, &mut nulls, &mut n, Vec::new(), usize::MAX).is_err()); // alloc: Cold
    }
}

//! The hosted emit (copied from gravity_lower::hosted at ladder-final): a tier B / C site's call cells, the by-reference
//! result region per lane and the raise stash; the programs and cells outlive the IR with the `Program` (`Descs::owner`).
#![allow(unsafe_code)]

use gravity_abi::{word, PtrWord, Word};
use core::any::Any;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use gravity_expr::emit::{Cell, EmitState, RowRefs};
use gravity_expr::nodes::func::How;
use gravity_expr::nodes::hostcall::{self, HostCall};
use gravity_expr::percode::CodeCol;
use gravity_expr::Compiled;
use types_core::fmgr::AggFnArgTypes;
use types_core::Oid;
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};

use gravity_abi::ctx::hdr;
use gravity_abi::program::prog;
use gravity_ir::{ConvOp, HostFlags, HostFn, IntCond, Type, Value};
use mcx::{reserved_in, GrowIn, Mcx, PgVec, PinnedContext};


/// A compiled expression and its parameter words: `places[i]` is where need `needs[i]` reads (`WordPlan`).
pub struct Ex<'p> {
    pub c: Compiled<'p>,
    pub places: Vec<gravity_lower::Place>,
    pub needs: Vec<gravity_expr::emit::ParamNeed>,
    /// A compile-time attachment the expression's compiler reads back at emit (step/expr.rs: its per-code plan).
    pub ext: Option<std::rc::Rc<dyn core::any::Any>>,
    /// The frame words holding its result on a row a stopped call re-enters (kind expr.volatile, face/step.rs `Held`).
    pub held: Vec<i32>,
    /// It holds a hosted call read as VOLATILE, fail closed (effects/visible/expr.rs `volatile_site`), decided where it is
    /// placed (frame/px.rs `add_placed`), since its readers at emit hold no catalog; a claim operand's also by its plan
    /// node's shared reading (effects/visible/expr.rs `operand`).
    pub volatile: bool,
    /// D3033: its operands over literals and nest parameters alone (`frame/hoist.rs` `late_roots`, decided where it is
    /// placed): computed once at its hoist scope, not per row, under the held-error licence (construct C2: a raise is
    /// held until a row reaches the operand).
    pub late: Option<(gravity_decompose::effects::Licence, Vec<gravity_expr::NodeId>)>,
}

const BASE_BYTES: usize = 64 << 10; // D848: a lane's result region's base, uninitialized (was 8 MiB zeroed); spills from 16 x
/// D3990: a result this wide or wider is a big one (`Lane::big`). Narrower results keep the window's batch, as before.
const BIG_RESULT: usize = BASE_BYTES;
/// D641: a spill chunk's cap: PostgreSQL's `MaxAllocSize` (memutils.h, the largest datum a palloc takes) and the image's
/// slack.
const SPILL_MAX: usize = mcx::MAX_ALLOC_SIZE + 1 + (1 << 20);
const _: () = assert!(SPILL_MAX == (1 << 30) + (1 << 20));
use crate::rep::INTERNALOID;

pub fn refused(c: &Compiled<'_>) -> Option<(&'static str, HostCall)> {
    for s in c.prog.sites.iter().filter(|s| !gravity_expr::emit::prim::is_prim(&c.prog, s, false)) {
        let h = hostcall::of(s);
        if let How::Fmgr(ix) = s.how {
            let f = &c.state.call_facts[ix as usize];
            if f.node_arg && f.argtypes.iter().any(|&t| t == INTERNALOID) {
                return Some(("an internal-typed argument to a polymorphic call", h));
            }
        }
        let what = match h.reason.split(':').next().unwrap_or("") {
            "set-returning" => "a set-returning function called as a scalar",
            "window" => "a window function called as a scalar",
            "handler/internal" => "a handler or internal function",
            _ => continue,
        };
        return Some((what, h));
    }
    None
}

/// The owner, reached through `ProgramBlock::owner`: the four leading fields are lane 0's (the leader's); `hosted_enter` hands a function its worker ordinal's lane.
#[repr(C)]
struct Owner {
    cells: Word<*mut Word<*mut Cell>>,
    region: Word<*mut u8>,
    region_end: Word<*mut u8>,
    states: Word<*mut EmitState<'static>>,
    n: u64,
    hold: Hold,
    lanes: Lanes,
    /// The root's owner as written (`owner_of`: the statement `hosted_root` gave it; `adopt`: the holder's), on the leader
    /// with no worker inside: a lane past 0 takes it for its own root at its arming (`arm_lane`), on whichever thread.
    holder: core::cell::Cell<memledger::Owner>,
    /// planprod P4: the owner's product root ('GravityHosted'): the statement's while the lowering builds it
    /// (`hosted_root`), adopted by the holder that files the program (`adopt`). Last: everything above drops first.
    root: PinnedContext,
}

/// planprod P4: a hosted owner's product root, owned by the running statement from its birth (by construction, never the
/// thread's guess): the program cache adopts it when it files the program (`adopt`).
pub(crate) fn hosted_root() -> PinnedContext {
    let root = PinnedContext::new(gravity_driver::mem::root::context("GravityHosted", true));
    root.set_owner(memledger::charge::statement_owner());
    root
}

/// planprod P4: the holder filing a program adopts its hosted owner (`Program::owner`, `finish_keeping`'s or `rebuild`'s):
/// the owner's product root and every context under it are `o`'s from now (mcx `set_owner_tree`); nothing for another owner.
/// The lanes past 0 armed so far re-own their roots too (`Lanes::reown`), and the ones armed later take `o` at birth.
/// planprod P6: so is the root of the template the owner's lowering made (`Progs::Tpl`'s `built`), until the program
/// caches hold it (`publish`).
pub fn adopt(owner: &dyn Any, o: memledger::Owner) {
    if let Some(ow) = owner.downcast_ref::<Owner>() {
        ow.root.set_owner_tree(o);
        ow.holder.set(o);
        ow.lanes.reown(o);
        if let Progs::Tpl(t, _, true) = &ow.hold.programs {
            if let Some(t) = t.downcast_ref::<HostTpl>() {
                t.reown(o);
            }
        }
    }
}

/// The program caches' memory account (gravity_backend `cache::PROGRAM_CACHES`).
const PROGRAM_CACHES: memledger::Owner = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::ProgramCaches));

/// planprod P6: the process-wide tier holds a program filed without its owner (exec admit, beside the program's own
/// `file_on_ledger`): its template's root (`template`) is the program caches' from now until its last drop, on whichever
/// thread. Once: a template another backend's tier entry already holds stays where it is.
pub fn publish(tpl: &(dyn Any + Send + Sync)) {
    if let Some(t) = tpl.downcast_ref::<HostTpl>() {
        t.publish(PROGRAM_CACHES);
    }
}

/// What the owner holds; every list here lives in the owner's product root (`Owner::root`), which drops after it.
struct Hold {
    states: PgVec<'static, EmitState<'static>>,
    programs: Progs,
    region: PgVec<'static, core::mem::MaybeUninit<u8>>,
    argtypes: PgVec<'static, Box<[Oid]>>,
    cells: PgVec<'static, Word<*mut Cell>>,
    keep: Option<Box<dyn Any>>,
    /// B2: the types and encoding pairs the catalog-armed sites reach (`gravity_fn::pool_catalog`): exec resolves them
    /// on the leader per run.
    catalog: gravity_fn::pool_catalog::Reach<'static>,
}

/// The hosted expressions' compiled programs, which the cells point into: the lowering's own (their lists in the owner's
/// product root, `raw::hosted_program`), or (D3438) a template's immutable programs beside this backend's
/// state of each (its function-manager calls). planprod P6: `built`, the template is the one this owner's lowering made
/// (`make_template`), whose root `adopt` re-owns; a rebuilt owner (`rebuild`) shares one another owner made.
enum Progs {
    /// The programs, and why no template stands for them (`shared_descs`).
    Own(#[allow(dead_code)] PgVec<'static, gravity_expr::HostedCompiled>, &'static str),
    Tpl(std::sync::Arc<dyn Any + Send + Sync>, #[allow(dead_code)] Vec<gravity_expr::State>, bool),
}

// the template's `Send + Sync` claim (lint R6)
mod raw;
use raw::{HostTpl, Lanes};

struct TplCall {
    funcid: Oid,
    nargs: usize,
    collation: Oid,
    facts: gravity_expr::catalog::CallFacts,
    tracked: bool,
}

/// One worker ordinal's cells and region: lane 0 the owner's own, the others armed at a worker's first entry (once per lane per program); a lane past 0 reaches only sites `pool_runs` admits (`pool_serial`).
#[repr(C)]
struct Lane {
    cells: Word<*mut Word<*mut Cell>>,
    region: Word<*mut u8>,
    region_end: Word<*mut u8>,
    states: Word<*mut EmitState<'static>>,
    n: u64,
    hold: Option<Box<LaneHold>>,
    /// D641: the Call's bound ctx (`h_enter`) and the result region's spill chunks.
    ctx: Word<*mut gravity_abi::Ctx>,
    spills: Spills,
    /// B2: the claim generation (`gravity_fn::session::claim_gen`) this lane's twins last ran under. A pool lane outlives
    /// its statement with the cached program, and a twin's `fn_extra` may hold what the claim's armed catalog images
    /// built (a typcache entry, an enum's order): those images go with their statement, so a lane entered under another
    /// generation drops every twin's `fn_extra` first (C's per-query `fmgr_info`).
    gen: u64,
    /// D3531: this thread's catalog views when its twins last ran (gravity_deps `Opaque`: a twin's `fn_extra` is state
    /// no recording sees). A cached program's lane keeps its twins' `fn_extra` across statements; a memo built from a
    /// catalog row a later invalidation replaced (jsonb_populate_record's tuple descriptor of a composite ALTER TYPE
    /// changed) goes at the lane's first entry of the next statement, as C's per-query `fmgr_info` rebuilt it. Never
    /// mid-statement: C keeps a query's memos.
    cat: gravity_deps::Opaque,
    /// rearm E4: the `track_functions` its cells were last retracked under (`NO_TRACK` before the first entry).
    track: i32,
    /// D3990: the slot (`gravity_fn::Region`'s address; 0 none) whose result of `BIG_RESULT` bytes or more spilled first since
    /// the IR last cleared it (a row's start, `Ir::give_back_big`), and that slot's `[cur, end]` before the spill: an end
    /// that copies its row gives the bytes from there back, so a window of wide results is never live at once.
    big_slot: u64,
    big_cur: u64,
    big_end: u64,
}

impl Lane {
    /// A lane no worker has entered: `h_enter` arms it at its first entry (`arm_lane`).
    fn idle() -> Lane {
        Lane { cells: word(core::ptr::null_mut()), region: word(core::ptr::null_mut()), region_end: word(core::ptr::null_mut()), states: word(core::ptr::null_mut()), n: 0, hold: None, ctx: word(core::ptr::null_mut()), spills: NO_SPILLS, gen: 0, cat: gravity_deps::Opaque::NONE, track: NO_TRACK, big_slot: 0, big_cur: 0, big_end: 0 }
    }
}

const NO_TRACK: i32 = i32::MIN;

/// D641: a lane's spill chunks, drawn from the statement's grant (`spill_region`) and held by the driver until the statement's
/// end (its settle drops them; `epoch` then moves). A chunk belongs to the region slot writing into it; the slot's rewind to
/// the base (seen at its next spill, or at its function's entry) frees its chunks for reuse, never to the allocator, so no
/// chunk goes while a slot of the same Call may still read it.
struct Spills {
    epoch: u64,
    last: usize,
    chunks: Vec<Chunk>,
}

struct Chunk {
    at: usize,
    bytes: usize,
    /// The slot's address (`gravity_fn::Region`) that writes it; 0 free.
    owner: usize,
}

const NO_SPILLS: Spills = Spills { epoch: 0, last: 0, chunks: Vec::new() };

/// A lane past 0's own state (`arm_lane`): its twin states, their cells and its result region in the lane's root
/// ('GravityHostedLane', `raw::owned_root`), owned at birth by its owner's holder (`Owner::holder`). The function-manager
/// twins and each twin state's own cell buffer stay on the heap.
struct LaneHold {
    states: PgVec<'static, EmitState<'static>>,
    cells: PgVec<'static, Word<*mut Cell>>,
    region: PgVec<'static, core::mem::MaybeUninit<u8>>,
    calls: Vec<Box<gravity_fn::FmgrCall>>, // M8 (D1036): a nested lane's own function-manager calls (`arm_lane`)
    /// Last: the lists above drop first.
    root: mcx::SendRoot,
}

/// One lane per worker ordinal the driver hands out: the workers' (below `MAX_WIDTH` + 1) and, on top, the nested lanes
/// (M8, D1036); the table follows the driver's ordinal space, not a count of its own (CAP-18).
const LANES: usize = gravity_driver::NESTED_LANE_TOP as usize + 1;
const _: () = assert!(gravity_driver::MAX_WIDTH < gravity_driver::NESTED_LANE_FIRST && gravity_driver::NESTED_LANES < gravity_driver::NESTED_LANE_TOP);
const L_CELLS: i32 = core::mem::offset_of!(Lane, cells) as i32;
const L_REGION: i32 = core::mem::offset_of!(Lane, region) as i32;
const L_REGION_END: i32 = core::mem::offset_of!(Lane, region_end) as i32;
const L_BIG_SLOT: i32 = core::mem::offset_of!(Lane, big_slot) as i32;
const L_BIG_CUR: i32 = core::mem::offset_of!(Lane, big_cur) as i32;
const L_BIG_END: i32 = core::mem::offset_of!(Lane, big_end) as i32;

/// One function's hosted state, built before its IR (the cells' addresses are the IR's constants).
pub struct Hosted<'m> {
    states: Vec<EmitState<'m>>,
    /// Lane 0 gets a result region of `BASE_BYTES` (in the owner's root, `owner_of`).
    base: bool,
    argtypes: Vec<Box<[Oid]>>,
    map: Vec<(usize, usize)>,
    sig: Vec<u32>,
    stamps: Vec<(Oid, [u32; 3])>,
    any: bool,
    /// D3620: no site writes into the result region (`byval_only`): the owner's lane 0 has no base (`owner_of`'s `lazy`).
    lazy: bool,
    windows: bool,
    /// D3583: per expression (`map`'s index), whether one of its cells is a function-manager call (a window `Ir::reset_seen` resets).
    fmgr: Rc<[bool]>,
    catalog: gravity_fn::pool_catalog::Reach<'m>,
}

impl<'m> Hosted<'m> {
    pub fn new(exs: &mut [&mut Ex<'_>], scratch: Mcx<'m>) -> Hosted<'m> {
        let any = exs.iter().any(|e| e.c.host_calls() > 0);
        Self::build(exs, any, scratch)
    }

    /// `new` over a scan with code lanes: an expression that is one per-code root (`phrase <> ''` against the
    /// empty code) is evaluated per entry, never through a cell per row — the row loop emits it over `codes` (rows.rs).
    pub fn new_over(exs: &mut [&mut Ex<'_>], codes: &[CodeCol], scratch: Mcx<'m>) -> Hosted<'m> {
        Self::new_over_bodies(exs, codes, 0, scratch)
    }

    /// `new_over` whose last `nbodies` expressions are per-code bodies the program sweeps itself (lowerv2 text/codes.rs): a
    /// hosted site in one of them is called once per dictionary entry from the program, so it needs the cells too.
    pub fn new_over_bodies(exs: &mut [&mut Ex<'_>], codes: &[CodeCol], nbodies: usize, scratch: Mcx<'m>) -> Hosted<'m> {
        let n = exs.len() - nbodies;
        let any = exs[..n].iter().any(|e| e.c.host_calls() > 0 && !e.c.percode_whole(codes, scratch)) || exs[n..].iter().any(|e| e.c.host_calls() > 0);
        Self::build(exs, any, scratch)
    }

    /// `scratch`: the planning context the islands' signature prints in.
    fn build(exs: &mut [&mut Ex<'_>], any: bool, scratch: Mcx<'m>) -> Hosted<'m> {
        let mut states = Vec::with_capacity(if any { exs.len() } else { 0 }); // alloc: Program
        let mut argtypes: Vec<Box<[Oid]>> = Vec::new(); // alloc: Program
        let mut map = Vec::new(); // alloc: Program
        let mut sig = Vec::new(); // alloc: Program
        let mut stamps = Vec::new(); // alloc: Program
        let mut catalog = gravity_fn::pool_catalog::Reach::new_in(scratch);
        if any {
            for (i, e) in exs.iter_mut().enumerate() {
                map.push((e.c.prog.sites.as_ptr() as usize, i));
                sig.extend(e.c.prog.sites.iter().flat_map(|s| [s.funcid, s.collation, s.nargs as u32]));
                sig.push(u32::MAX);
                for s in e.c.prog.sites.iter().filter(|s| gravity_decompose::effects::user_code(s.funcid)) {
                    if let Ok(Some((xmin, tid))) = fmgr_core::proc_stamp(s.funcid) {
                        let st = stamp_words(xmin, (tid.ip_blkid.bi_hi, tid.ip_blkid.bi_lo, tid.ip_posid));
                        sig.extend(st.into_iter().chain([u32::MAX - 4]));
                        stamps.push((s.funcid, st));
                    }
                }
                // the carrier's types are the owner's, not the IR's: `to_json(date)` after `to_json(timestamptz)` is another program
                // ... and the call's VARIADIC bit: `json_build_object(VARIADIC arr)` after `json_build_object(arr)` is another
                // program (`get_fn_expr_variadic` reads it from the carrier; the reused program handed the array as one argument)
                for f in e.c.state.call_facts.iter() {
                    sig.push(f.rettype);
                    sig.extend_from_slice(&f.argtypes);
                    sig.push(f.variadic as u32);
                    sig.push(u32::MAX - 1);
                }
                // B2: an admitted catalog body's argument and result types (whether or not its pipeline runs parallel:
                // imaging a type a serial site reaches is harmless)
                for s in e.c.prog.sites.iter() {
                    if let How::Fmgr(ix) = s.how {
                        if gravity_fn::pool_catalog::audited(s.funcid) {
                            // only a site `catalog_site` admits: one whose closure cannot be imaged stays serial and
                            // must not make the run's build refuse
                            if let Some(r) = catalog_site(&e.c, s, ix as usize) {
                                // B2b: a conversion site's pair (none for an unknown name) is the owner's, as its
                                // types are: the IR binds the encoding name as a word, so `convert_to(t, 'WIN1252')`
                                // after `convert_to(t, 'LATIN1')` is the same IR and must be another program
                                if gravity_fn::pool_catalog::converts(s.funcid) {
                                    r.convs().iter().for_each(|&(a, b)| sig.extend([a as u32, b as u32]));
                                    sig.push(u32::MAX - 5);
                                }
                                catalog.add(&r);
                            }
                        }
                    }
                }
                for d in e.c.state.descs.iter() {
                    d.sig(&mut sig);
                    sig.push(u32::MAX - 2);
                }
                if let Some(fp) = gravity_expr::percode::islands_sig(&e.c.prog, scratch) {
                    sig.extend([fp as u32, (fp >> 32) as u32, u32::MAX - 3]);
                }
                for (call, f) in e.c.state.calls.iter_mut().zip(e.c.state.call_facts.iter()) {
                    sig.push(call.tracked() as u32);
                    carrier(call, f, &mut argtypes);
                }
                states.push(e.c.emit_state(scratch));
            }
        }
        let lazy = any && !exs.is_empty() && exs.iter().all(|e| byval_only(&e.c));
        let base = any && !lazy;
        let fmgr: Rc<[bool]> = states.iter().map(|s: &EmitState<'_>| s.heads().any(|c| !c.fmgr.ptr().is_null())).collect(); // alloc: Program
        let windows = fmgr.iter().any(|&f| f); // a function-manager window somewhere: the per-row / per-push reset is emitted only then (lane F1F3)
        Hosted { states, base, argtypes, map, sig, stamps, any, lazy, windows, fmgr, catalog }
    }

    pub fn any(&self) -> bool {
        self.any
    }

    pub fn declare(&self, pb: &mut ProgramBuilder) -> Ir {
        let flags = HostFlags { touches_memory: true, ..HostFlags::default() };
        let (raise_h, enter_h, reset_h) = if self.any {
            (Some(pb.declare_host("hosted_raise", &[Type::Ptr], Some(Type::I64), h_raise as *const () as usize, flags)), Some(pb.declare_host("hosted_enter", &[Type::Ptr, Type::I64, Type::Ptr, Type::Ptr], Some(Type::Ptr), h_enter as *const () as usize, flags)), if self.windows { Some(pb.declare_host("hosted_reset", &[Type::Ptr], Some(Type::I64), h_reset as *const () as usize, flags)) } else { None })
        } else {
            (None, None, None)
        };
        // without windows every entry is false: `get(i).unwrap_or(false)` reads the same
        let fmgr = self.fmgr.clone(); // alloc: Program
        Ir { raise_h, enter_h, reset_h, map: self.map.clone(), fmgr, seen: RefCell::new(Vec::new()), owner: None, slot: None, base: None, end: None, lane: None, cells: None, refd: core::cell::Cell::new(0) } // alloc: Program
    }

    /// After the IR: the programs move out of their `Ex` and the state is boxed for the `Program`; the owner also holds
    /// `keep`: a rung's own per-program state beside the call cells (srf.rs's function-manager
    /// sets). planprod P4: in `root`, the lowering's product root when it made one already (`Px::program_cx`'s contexts
    /// are its children), else a new one (`hosted_root`). memsig E-prog: `lists` is the root the programs' lists were
    /// built in (`Px::lists_root`, the statement's): the filing compacts each list into the holder's root (a template's
    /// or the owner's product root, `Program::into_hosted`), and `lists` drops after, with none of the growth's slack.
    pub fn finish_keeping<'p>(self, exs: impl IntoIterator<Item = Ex<'p>>, keep: Option<Box<dyn Any>>, root: Option<PinnedContext>, lists: Option<mcx::SendRoot>) -> Option<(Rc<dyn Any>, Vec<u32>)> {
        // an owner that keeps anything is always an `Owner`, so `kept` reads it whatever sites the program has (V4R-101)
        if !self.any && keep.is_none() {
            // the programs (their lists in `lists`) drop first
            exs.into_iter().for_each(drop);
            drop(lists);
            return None;
        }
        let mut programs = Vec::new(); // alloc: Program
        for e in exs {
            programs.push(e.c);
        }
        let mut states = self.states;
        let (programs, root) = match shared_descs(&programs, keep.as_deref(), self.any) {
            Ok(descs) => (make_template(raw::for_template(programs), &mut states, descs, self.stamps, self.any, self.lazy, &self.catalog), root),
            // Each program's lists move into the owner's product root, the cells re-pointed at them
            Err(why) => {
                let root = root.unwrap_or_else(hosted_root);
                let m = raw::product_mcx(&root);
                let mut own = reserved_in(programs.len(), m);
                own.extend_in(programs.into_iter().map(|c| raw::hosted_program(c, m)));
                for (s, c) in states.iter_mut().zip(own.iter_mut()) {
                    s.rebind_all(c);
                }
                (Progs::Own(own, why), Some(root))
            }
        };
        // memsig E-prog: every list is compacted out of the lowering's lists root: it drops now
        drop(lists);
        Some((owner_of(states, programs, self.base, self.argtypes, keep, &self.catalog, self.lazy, root), self.sig))
    }
}

/// D3620: whether no site of `c` writes into the result region: every site's result is by value (a shim's `byref` image
/// flag, a function-manager call's result type) and none is an island. Such an owner gets no base region (`owner_of`'s
/// `lazy`); a result that reaches it anyway spills into a chunk of the statement's grant, so a misjudged site costs
/// memory, never an answer.
fn byval_only(c: &Compiled<'_>) -> bool {
    c.prog.sites.iter().all(|s| match s.how {
        How::Shim { byref, .. } => !byref,
        How::Fmgr(_) => gravity_fn::typesem::storage::len_byval(s.rettype).is_ok_and(|(_, byval)| byval),
        How::Island { .. } => false,
    })
}

/// A function-manager call's carrier (C `fmgr_info_set_expr` over the calling node): its argument and result types, kept in
/// `argtypes` with the owner.
fn carrier(call: &mut gravity_fn::FmgrCall, f: &gravity_expr::catalog::CallFacts, argtypes: &mut Vec<Box<[Oid]>>) {
    let types: Box<[Oid]> = f.argtypes.clone().into_boxed_slice();
    // SAFETY: the box lives in the owner, with the program, longer than the call that reads it.
    let slice = unsafe { core::mem::transmute::<&[Oid], &'static [Oid]>(&types[..]) };
    argtypes.push(types);
    // the call's collation is its input collation (C ExecInitFunc hands `inputcollid` to the call), what
    // `exprInputCollation` gives funcapi's polymorphic OUT columns
    let inputcollid = call.collation();
    call.set_fn_expr(AggFnArgTypes { rettype: f.rettype, argtypes: slice, variadic: f.variadic, stable: f.stable, inputcollid });
}

/// A user function's pg_proc row stamp as the sig keys it.
fn stamp_words(xmin: types_core::TransactionId, (bi_hi, bi_lo, posid): (u16, u16, u16)) -> [u32; 3] {
    [xmin, (bi_hi as u32) << 16 | bi_lo as u32, posid as u32]
}

/// D3438: the IN-list descriptors of an owner a template can stand for (empty when it keeps none), or why none can (the
/// admit report's `tier=private:host.<why>`, `private_why`): an island (`island`), a SQL/JSON slot (`json`) or a row
/// descriptor (`descs`) (their state is the backend's or points into the plan), byte images (`data`, `Px::data`), or an
/// owned object other than an IN-list descriptor (`owned*`: `owned_why`, `keep`) keeps the program its backend's; so does a call
/// `Hosted::build` gave no carrier (`calls`: no hosted site of the owner reaches it, `any` false).
fn shared_descs(programs: &[Compiled<'_>], keep: Option<&dyn Any>, any: bool) -> Result<Vec<std::sync::Arc<crate::step::raw::saop_host::Desc>>, &'static str> {
    let why = |c: &Compiled<'_>| {
        if !c.prog.islands.is_empty() || c.prog.sites.iter().any(|s| matches!(s.how, How::Island { .. })) {
            Some("island")
        } else if !c.prog.json_vars.is_empty() || !c.state.json.is_empty() {
            Some("json")
        } else if !c.state.descs.is_empty() {
            Some("descs")
        } else if !any && !c.state.calls.is_empty() {
            Some("calls")
        } else {
            None
        }
    };
    if let Some(w) = programs.iter().find_map(why) {
        return Err(w);
    }
    let Some(k) = keep else { return Ok(Vec::new()) };
    let (data, owned) = k.downcast_ref::<(Vec<Box<[u8]>>, Vec<Box<dyn Any>>)>().ok_or("keep")?;
    if !data.is_empty() {
        return Err("data");
    }
    owned.iter().map(|o| o.downcast_ref::<std::sync::Arc<crate::step::raw::saop_host::Desc>>().cloned()).collect::<Option<_>>().ok_or_else(|| owned_why(owned))
}

/// Which owned object keeps an owner its backend's (`shared_descs`): a program context (`Px::program_cx`) by its name,
/// whose nodes point into the statement's plan (`owned.mergejoin`: the markable merge join's compare calls over the
/// clauses' plan arguments; `owned.wholerow`: a whole-row record's), else `owned`.
fn owned_why(owned: &[Box<dyn Any>]) -> &'static str {
    let cx = owned.iter().filter(|o| !o.is::<std::sync::Arc<crate::step::raw::saop_host::Desc>>()).find_map(|o| o.downcast_ref::<mcx::MemoryContext>());
    match cx.map(mcx::MemoryContext::name) {
        Some("merge.join compares") => "owned.mergejoin",
        Some("expr.wholerow record") => "owned.wholerow",
        _ => "owned",
    }
}

/// Why no template stands for `owner` (`shared_descs`' reason; the admit report's `tier=private:host.<why>`): "" when one
/// does, `kind` for an owner of another kind.
pub fn private_why(owner: &dyn Any) -> &'static str {
    match owner.downcast_ref::<Owner>().map(|o| &o.hold.programs) {
        Some(Progs::Own(_, why)) => *why,
        Some(Progs::Tpl(..)) => "",
        None => "kind",
    }
}

/// D3438: the owner's programs as a template's (`HostTpl`), with each one's state this backend's. planprod P6: the
/// programs' lists move into the template's root, and the owner's `cells` (built over them where they were) are re-pointed
/// there; no template program has an island (`shared_descs`).
fn make_template(programs: Vec<Compiled<'static>>, cells: &mut [EmitState<'_>], descs: Vec<std::sync::Arc<crate::step::raw::saop_host::Desc>>, stamps: Vec<(Oid, [u32; 3])>, any: bool, lazy: bool, catalog: &gravity_fn::pool_catalog::Reach<'_>) -> Progs {
    let (mut progs, mut states, mut calls) = (Vec::with_capacity(programs.len()), Vec::with_capacity(programs.len()), Vec::with_capacity(programs.len())); // alloc: Program
    let mut bytes = core::mem::size_of::<HostTpl>();
    for c in programs {
        let Compiled { prog, state, .. } = c;
        bytes += prog_bytes(&prog) + state.calls.len() * core::mem::size_of::<TplCall>();
        calls.push(state.calls.iter().zip(state.call_facts.iter()).map(|(k, f)| TplCall { funcid: k.oid(), nargs: k.nargs(), collation: k.collation(), facts: f.clone(), tracked: k.tracked() }).collect()); // alloc: Program
        progs.push(prog);
        states.push(state);
    }
    let tpl = HostTpl::new(progs, calls, stamps, any, lazy, catalog, descs, bytes);
    for ((s, p), st) in cells.iter_mut().zip(tpl.progs.iter()).zip(states.iter_mut()) {
        s.rebind_to(p, st);
    }
    Progs::Tpl(std::sync::Arc::new(tpl), states, true) // alloc: Program
}

/// A compiled program's heap bytes (a template's share of the process-wide tier's bound).
fn prog_bytes(p: &gravity_expr::Program<'_>) -> usize {
    use core::mem::size_of;
    p.nodes.capacity() * size_of::<gravity_expr::ExprNode<'_>>() + p.kids.capacity() * size_of::<gravity_expr::NodeId>() + p.sites.capacity() * size_of::<gravity_expr::nodes::func::Call>() + p.convs.capacity() * size_of::<gravity_expr::nodes::func::ArgConv>() + p.names.iter().map(|n| n.len() + 16).sum::<usize>() + p.oids.capacity() * 4 + p.owned.iter().map(|o| o.capacity() + 24).sum::<usize>()
}

/// The owner over `states` (their cells), lane 0 armed on `region`. D3620: `lazy` (no site writes into the region, and
/// `region` is empty): lane 0's base is an empty range at the lane's own address, registered as spillable, so a result
/// that does reach it (a misjudged site) spills into a chunk drawn from the statement's grant (`spill`, starting at
/// `BASE_BYTES`), and the owner holds no 64 KiB base (one resident page each; every level of a deep SQL-function
/// recursion holds its owner).
fn owner_of(states: Vec<EmitState<'_>>, programs: Progs, base: bool, argtypes: Vec<Box<[Oid]>>, keep: Option<Box<dyn Any>>, catalog: &gravity_fn::pool_catalog::Reach<'_>, lazy: bool, root: Option<PinnedContext>) -> Rc<dyn Any> {
    let root = root.unwrap_or_else(hosted_root);
    let m = raw::product_mcx(&root);
    let mut st = reserved_in(states.len(), m);
    st.extend_in(states.into_iter().map(|s| s.into_in(m))); // the cells move into the owner's root; each state's own buffer is where it is
    let mut cells = reserved_in(st.len(), m);
    cells.extend_in(st.iter_mut().map(|s| word(s.ptr())));
    let mut types = reserved_in(argtypes.len(), m);
    types.extend_in(argtypes);
    let catalog = catalog.clone_in(m);
    let region = raw::region_in(if base { BASE_BYTES } else { 0 }, m);
    let mut hold = Hold { states: st, programs, region, argtypes: types, cells, keep, catalog };
    let (rb, re) = (hold.region.as_mut_ptr().cast::<u8>(), hold.region.as_mut_ptr_range().end.cast::<u8>());
    let first = Lane { cells: word(hold.cells.as_ptr() as *mut Word<*mut Cell>), region: word(rb), region_end: word(re), states: word(hold.states.as_mut_ptr()), n: hold.states.len() as u64, hold: None, ctx: word(core::ptr::null_mut()), spills: NO_SPILLS, gen: 0, cat: gravity_deps::Opaque::NONE, track: NO_TRACK, big_slot: 0, big_cur: 0, big_end: 0 };
    // the owner `hosted_root` gave the root: the running statement's, on this (the lowering's) thread
    let holder = core::cell::Cell::new(memledger::charge::statement_owner());
    let mut owner = Owner { cells: word(hold.cells.as_ptr() as *mut Word<*mut Cell>), region: word(rb), region_end: word(re), states: word(hold.states.as_mut_ptr()), n: hold.states.len() as u64, hold, lanes: Lanes::new(first, m), holder, root };
    if lazy && rb == re {
        // the lane is boxed: its address holds for the owner's life
        let lane = &mut *owner.lanes.first;
        let at = (lane as *mut Lane).cast::<u8>();
        lane.region = word(at);
        lane.region_end = word(at);
        owner.region = word(at);
        owner.region_end = word(at);
        spillable(at, &mut *owner.lanes.first);
    } else if !re.is_null() && rb != re {
        spillable(re, &mut *owner.lanes.first);
    }
    Rc::new(owner) // alloc: Program
}

/// D3438: the template of `owner` (`finish_keeping`'s), when one stands for it: the program may then be filed without
/// its owner (`Program::hosted`), each backend rebuilding its own (`rebuild`).
pub fn template(owner: &dyn Any) -> Option<std::sync::Arc<dyn Any + Send + Sync>> {
    match &owner.downcast_ref::<Owner>()?.hold.programs {
        Progs::Tpl(t, _, _) => Some(t.clone()),
        Progs::Own(..) => None,
    }
}

/// D3438: `template`'s heap bytes, for the process-wide tier's bound.
pub fn template_bytes(t: &(dyn Any + Send + Sync)) -> usize {
    t.downcast_ref::<HostTpl>().map_or(0, |t| t.bytes)
}

/// `catalog_reach` of a program filed without its owner, from its template (D3438).
pub fn template_reach(t: &(dyn Any + Send + Sync)) -> Option<&gravity_fn::pool_catalog::Reach<'static>> {
    t.downcast_ref::<HostTpl>().map(|t| &t.catalog)
}

/// D3438: this backend's owner of a program filed without one, from its template (`template`): each call bound here,
/// with its carrier, cells, result region and lanes of its own. `None` (a miss: the caller lowers its own) when a user
/// function's row changed since the lowering, a call's function-usage tracking differs here (`track_functions`, which the
/// program's key holds) or a function no longer binds.
pub fn rebuild(tpl: &std::sync::Arc<dyn Any + Send + Sync>) -> Option<Rc<dyn Any>> {
    let t = tpl.downcast_ref::<HostTpl>()?;
    for &(f, st) in t.stamps.iter() {
        match fmgr_core::proc_stamp(f) {
            Ok(Some((xmin, tid))) if stamp_words(xmin, (tid.ip_blkid.bi_hi, tid.ip_blkid.bi_lo, tid.ip_posid)) == st => {}
            _ => return None,
        }
    }
    let scratch = mcx::MemoryContext::new_bump("GravityRebuild"); // the cells' first home: `owner_of` moves them into the owner's root
    let (mut states, mut cstates, mut argtypes) = (Vec::with_capacity(t.progs.len()), Vec::with_capacity(t.progs.len()), Vec::new()); // alloc: Program
    // as `Hosted::build`: cells only for a program with a hosted site (an owner of IN-list descriptors alone has none)
    for (prog, calls) in t.progs.iter().zip(t.calls.iter()).filter(|_| t.any) {
        let mut state = gravity_expr::State::empty();
        for c in calls.iter() {
            let mut call = gravity_fn::FmgrCall::bind(c.funcid, c.nargs, c.collation).ok()?;
            if call.tracked() != c.tracked {
                return None;
            }
            carrier(&mut call, &c.facts, &mut argtypes);
            state.calls.push(call); // alloc: Program
            state.call_facts.push(c.facts.clone()); // alloc: Program
        }
        states.push(EmitState::of_hosted(prog, &mut state, scratch.mcx())); // alloc: Program
        cstates.push(state); // alloc: Program
    }
    let keep = (!t.descs.is_empty()).then(|| Box::new((Vec::<Box<[u8]>>::new(), t.descs.iter().map(|d| Box::new(d.clone()) as Box<dyn Any>).collect::<Vec<_>>())) as Box<dyn Any>); // alloc: Program
    Some(owner_of(states, Progs::Tpl(tpl.clone(), cstates, false), t.any && !t.lazy, argtypes, keep, &t.catalog, t.any && t.lazy, None))
}

/// The first hosted site a pool thread cannot run (MH-plan §9.3 as corrected): an island, a function-manager site `pool_runs` does not
/// admit (a body over the backend's catalog or session state no claim carries), a user-defined function, or a builtin whose
/// `proparallel` is not `'s'`; a shim is pure over its arguments and the arm.
/// A row-valued IS [NOT] NULL is one too, though its site is a synthetic shim (gravity_expr nodes/field.rs `ROW_NULL_SITE` reads the rowtype from the backend's typcache).
/// So is a ConvertRowtypeExpr (D3542: nodes/tuple.rs `refreshed` re-reads both rowtypes there; on a pool thread the lookup
/// raised "type with OID .. does not exist").
pub fn pool_unsafe_site(c: &Compiled<'_>, parallel: &mut dyn FnMut(Oid) -> Option<u8>) -> Option<HostCall> {
    c.prog.sites.iter().filter(|s| !gravity_expr::emit::prim::is_prim(&c.prog, s, false)).find(|s| !pool_runs(c, s) || gravity_decompose::effects::user_code(s.funcid) || (s.funcid != 0 && parallel(s.funcid) != Some(b's'))).map(hostcall::of).or_else(|| {
        (gravity_expr::emit::scalar::row_tests(&c.prog) > 0).then_some(HostCall { oid: 0, name: "row null test", tier: gravity_fn::Tier::C, reason: "a row-valued IS [NOT] NULL (the rowtype from the backend's typcache)" })
    }).or_else(|| {
        (gravity_expr::emit::scalar::row_converts(&c.prog) > 0).then_some(HostCall { oid: 0, name: "row convert", tier: gravity_fn::Tier::C, reason: "a ConvertRowtypeExpr (both rowtypes re-read from the backend's typcache)" })
    })
}

/// A site a pool thread can run: a shim (pure over its arguments and the claim's arm), or a function-manager site whose
/// builtin `gravity_fn::session::pool_hosted` admits (each pool lane binds its own copy of the call, `arm_lane`, and the
/// claim carries the session state the body reads, hosts `SessionCarry`). An interpreter island never is.
/// B2: or an audited catalog body (`gravity_fn::pool_catalog`) whose argument and result types image whole on the leader
/// (no record, composite or domain in the closure; every procedure it resolves an fmgr builtin): the claim arms the
/// statement's images (hosts `SessionCarry`), so the body's typcache reads never reach a catcache the thread lacks. A
/// site whose closure cannot be imaged stays serial, as before.
fn pool_runs(c: &Compiled<'_>, s: &gravity_expr::nodes::func::Call) -> bool {
    match s.how {
        How::Shim { .. } => true,
        How::Fmgr(ix) => gravity_fn::session::pool_hosted(s.funcid) || catalog_site(c, s, ix as usize).is_some(),
        How::Island { .. } => false,
    }
}

/// B2: what an audited site reaches when its whole reach resolves on the leader (else None: the site stays serial): its
/// argument and result types and, for a conversion body, the encoding pair its constant encoding name(s) select (a name
/// that is not a constant keeps it serial; the value is read as a program decision, so it keys the program).
fn catalog_site(c: &Compiled<'_>, s: &gravity_expr::nodes::func::Call, ix: usize) -> Option<gravity_fn::pool_catalog::SiteReach> {
    if !gravity_fn::pool_catalog::audited(s.funcid) {
        return None;
    }
    let f = c.state.call_facts.get(ix)?;
    let mut reach = gravity_fn::pool_catalog::SiteReach::of_types(f.argtypes.iter().copied().chain([f.rettype]))?;
    if gravity_fn::pool_catalog::converts(s.funcid) {
        let kids = c.prog.kids.get(s.args.start as usize..(s.args.start + s.args.len) as usize)?;
        let mut encs = [0i32; 2];
        let names = kids.get(1..)?;
        if names.is_empty() || names.len() > encs.len() {
            return None;
        }
        for (k, e) in names.iter().zip(encs.iter_mut()) {
            match c.prog.nodes.get(k.0 as usize)? {
                gravity_expr::nodes::ExprNode::Const(k) if !k.is_null() && k.typ == NAMEOID => *e = gravity_fn::pool_catalog::name_encoding(k.read().value.as_usize()),
                _ => return None,
            }
        }
        if let Some(p) = gravity_fn::pool_catalog::convert_pair(s.funcid, &encs[..names.len()]) {
            reach.push_conv(p);
        }
    }
    matches!(gravity_fn::pool_catalog::StatementCatalog::build_site(&reach), Ok(Some(_))).then_some(reach)
}

const NAMEOID: Oid = 19;

/// B2: what a lowered program's catalog-armed sites reach (empty for any other program), from its owner
/// (`Program::owner`): exec resolves it on the leader for the run (`gravity_fn::pool_catalog::StatementCatalog::build`).
pub fn catalog_reach(owner: &dyn Any) -> Option<&gravity_fn::pool_catalog::Reach<'static>> {
    owner.downcast_ref::<Owner>().map(|o| &o.hold.catalog)
}

/// M5 L0-d (R8-L04, hazard H9a): an SRF step's functions run on the pool threads only when srf.parallel serves the
/// pipeline and every function is a set the iterator emits with no host (the int series: a library entry's or an array
/// walk's per-open context is the leader's slot bank), parallel safe and not volatile; any other set or the road off keeps
/// the pipeline on the leader (source/heap.rs and source/pgrc.rs ask it beside `pool_unsafe_site`).
pub fn srf_serial(px: &crate::frame::px::Px<'_, '_>, k: usize) -> bool {
    let road = px.strategies.has(crate::face::strategy::SRF_PARALLEL) && crate::face::source::on(crate::admit::refusal::Variant::Strategy(crate::face::strategy::SRF_PARALLEL));
    let facts = px.gx.facts;
    let pool_safe = |f: &gravity_decompose::ops::shared::srf::SrfFn<'_>| matches!(f.kind, gravity_decompose::ops::shared::srf::SrfKind::SeriesInt { .. }) && crate::effects::visible::core_face::pool_safe(facts, f.funcid);
    px.gx.pipelines()[k].ops.iter().any(|op| matches!(op, gravity_decompose::Op::Srf(d) if !road || !d.functions.iter().all(pool_safe)))
}

/// Pipeline `k`'s serial reason from its hosted calls and set functions: a call a pool thread cannot run (the shared
/// predicate's `StepShape::HostedCall`, which the planner's width model reads from a step's quals and targets) or a serial
/// SRF keeps it on the leader (source/heap.rs, source/pgrc.rs and a spill pair's kept source ask it).
pub fn hosted_serial(px: &crate::frame::px::Px<'_, '_>, k: usize) -> Option<gravity_program::Reason> {
    let catalog = px.gx.catalog;
    // D3585: read once per expression over the program's lowering (`Px::hosted_unsafe`), not again for each source
    let pool_safe = !px.hosted_unsafe(&|c| c.host_calls() > 0 && pool_unsafe_site(c, &mut |f| catalog.func(f).ok().and_then(|x| x.parallel)).is_some());
    gravity_program::step_serial(&gravity_program::StepShape::HostedCall { pool_safe }).or_else(|| srf_serial(px, k).then_some(gravity_program::Reason::Host))
}

/// The composer's serial word: a hosted expression of `exs` (not one the per-code road answers whole) has a site the pool cannot run.
pub fn pool_serial(exs: &[&mut Ex<'_>], codes: &[CodeCol], parallel: &mut dyn FnMut(Oid) -> Option<u8>, scratch: Mcx<'_>) -> bool {
    exs.iter().any(|e| e.c.host_calls() > 0 && !e.c.percode_whole(codes, scratch) && pool_unsafe_site(&e.c, parallel).is_some())
}

pub struct Ir {
    raise_h: Option<HostFn>,
    enter_h: Option<HostFn>,
    reset_h: Option<HostFn>,
    map: Vec<(usize, usize)>,
    /// D3583: `Hosted::fmgr` (empty without a function-manager window).
    fmgr: Rc<[bool]>,
    /// D3583: the expressions `with` gave their cells to, in lowering order, those with a function-manager window only
    /// (`seen` / `reset_seen`).
    seen: RefCell<Vec<usize>>,
    owner: Option<Value>,
    slot: Option<Value>,
    base: Option<Value>,
    end: Option<Value>,
    lane: Option<Value>,
    cells: Option<Value>,
    /// D3990: the host sites lowered so far (`refs`): a row that lowered none has nothing of its own in the region.
    refd: core::cell::Cell<u32>,
}

/// D3990: a row's start (`Ir::end_mark`): where the lane's big-result mark is cleared, if the row turns out to need it,
/// and the sites lowered before it.
pub struct RowMark {
    at: (gravity_ir::Block, usize),
    refd: u32,
    cleared: bool,
}

impl Ir {
    /// A function of a program with no hosted state.
    pub fn none() -> Ir {
        Ir { raise_h: None, enter_h: None, reset_h: None, map: Vec::new(), fmgr: Rc::from([]), seen: RefCell::new(Vec::new()), owner: None, slot: None, base: None, end: None, lane: None, cells: None, refd: core::cell::Cell::new(0) } // alloc: Program
    }

    /// The same program's hosts for another of its functions (`enter` gives it the function's own values).
    pub fn per_func(&self) -> Ir {
        Ir { raise_h: self.raise_h, enter_h: self.enter_h, reset_h: self.reset_h, map: self.map.clone(), fmgr: self.fmgr.clone(), seen: RefCell::new(Vec::new()), owner: None, slot: None, base: None, end: None, lane: None, cells: None, refd: core::cell::Cell::new(0) } // alloc: Program
    }

    /// The function's entry: the worker ordinal's lane armed and its windows reset, the region slot (`[cur, end]`) rewound to the lane's region.
    pub fn enter(&mut self, b: &mut FunctionBuilder<'_>) {
        let Some(h) = self.enter_h else { return };
        let ctx = b.params()[0];
        let pb = b.load(Type::Ptr, ctx, hdr::PROGRAM as i32);
        let owner = b.load(Type::Ptr, pb, prog::OWNER as i32);
        let ordinal = b.load_zext(Type::I16, ctx, hdr::WORKER_ORDINAL as i32);
        let slot = b.declare_stack_slot(16, 8);
        let r = b.stack_addr(slot);
        // every function-manager window reset: the previous call's rows left the out block (the block holds datum pointers into them)
        let Some(lane) = b.call_host(h, &[owner, ordinal, ctx, r], Some(Type::Ptr)) else { return };
        let base = b.load(Type::Ptr, lane, L_REGION);
        let end = b.load(Type::Ptr, lane, L_REGION_END);
        b.store(r, 0, base);
        b.store(r, 8, end);
        self.owner = Some(owner);
        self.slot = Some(r);
        self.base = Some(base);
        self.end = Some(end);
        self.lane = Some(lane);
        self.cells = Some(b.load(Type::Ptr, lane, L_CELLS));
    }

    /// The region rewound to the function's lane's base and every function-manager window reset: after a push consumed the out block (the sink), or per row when no tail holds a reference past the row (the fold); nothing for a program without host sites.
    pub fn rewind(&self, b: &mut FunctionBuilder<'_>) {
        let (Some(r), Some(base), Some(end), Some(lane)) = (self.slot, self.base, self.end, self.lane) else { return };
        b.store(r, 0, base);
        b.store(r, 8, end);
        if let Some(h) = self.reset_h {
            b.call_host(h, &[lane], Some(Type::I64));
        }
    }

    /// The reset at the unit `mem::reset_unit` names (the window, or the row).
    pub fn reset(&self, b: &mut FunctionBuilder<'_>) {
        self.rewind(b)
    }

    /// The region's top before a row whose renders a tail may hold: `release` gives the row's bytes back when the row is dropped instead (a filter's reject), the rows before it kept.
    /// The slot's `[cur, end]`: a row that spilled (D641) moved both, so `release` restores both.
    pub fn mark(&self, b: &mut FunctionBuilder<'_>) -> Option<(Value, Value)> {
        self.slot.map(|r| (b.load(Type::Ptr, r, 0), b.load(Type::Ptr, r, 8)))
    }

    pub fn release(&self, b: &mut FunctionBuilder<'_>, mark: (Value, Value)) {
        let (Some(r), Some(lane)) = (self.slot, self.lane) else { return };
        b.store(r, 0, mark.0);
        b.store(r, 8, mark.1);
        if let Some(h) = self.reset_h {
            b.call_host(h, &[lane], Some(Type::I64));
        }
    }

    /// D2670: a dropped row's bytes given back (the slot's top back at `mark`'s), PostgreSQL's per-tuple reset, while the
    /// slot is in the region it was in at the mark (`end` unchanged: every byte past the mark is the row's), or the mark is
    /// in the lane's base (D2074): then the slot goes back to the base even when the row spilled, and its next spill frees
    /// the row's chunks for reuse (`Spills::take`'s `in_base`), so rows that each spill reuse one chunk instead of drawing
    /// fresh ones to the window's end. A row that spilled from a chunk keeps its new chunk, so the next row's mark is in it.
    /// Unlike `release`, no function-manager window is reset: a window may hold a value a live row still reads (an outer
    /// row's NestLoopParam).
    pub fn give_back(&self, b: &mut FunctionBuilder<'_>, mark: (Value, Value)) {
        let Some(r) = self.slot else { return };
        let (cur, end) = (b.load(Type::Ptr, r, 0), b.load(Type::Ptr, r, 8));
        let (e0, e1) = (b.conv(ConvOp::PtrToInt, Type::I64, mark.1), b.conv(ConvOp::PtrToInt, Type::I64, end));
        let same = b.icmp(IntCond::Eq, e0, e1);
        let mut back = b.select(same, mark.0, cur);
        let mut back_end = end;
        if let Some(base_end) = self.end {
            let be = b.conv(ConvOp::PtrToInt, Type::I64, base_end);
            let in_base = b.icmp(IntCond::Eq, e0, be);
            back = b.select(in_base, mark.0, back);
            back_end = b.select(in_base, mark.1, end);
        }
        b.store(r, 0, back);
        b.store(r, 8, back_end);
    }

    /// D3990: at the start of a row whose end copies it (`EndMech::copies_row`): its place, no code (`None` without host sites).
    pub fn end_mark(&self, b: &FunctionBuilder<'_>) -> Option<RowMark> {
        self.lane?;
        self.slot?;
        Some(RowMark { at: b.cursor(), refd: self.refd.get(), cleared: false })
    }

    /// D3990: after that row's end. The row's start clears the lane's mark (`store_zero_at` there); the spill hook sets it at
    /// the row's first result of `BIG_RESULT` bytes or more: the slot and its `[cur, end]` before that spill. When set by
    /// this function's slot, it is cleared and the bytes from there are given back (`give_back`). With the mark in the
    /// lane's base, the next big result's spill reuses the row's chunk, so one window of wide results is never live at once
    /// (D3990: ~128 rows x 1.28 MB of repeat() results, held until the statement's end). Rows of narrow results never set
    /// it and keep the window's batch; a row that lowered no host site emits nothing. Sound only where the end copied
    /// every byte of the row it reads and the row reaches it once (no step expands a row or holds rows back): nothing
    /// after the end reads the row's region bytes past the mark. A nested function's slot sets it for its own rows only.
    pub fn give_back_big(&self, b: &mut FunctionBuilder<'_>, m: &mut RowMark) {
        let (Some(r), Some(lane)) = (self.slot, self.lane) else { return };
        if self.refd.get() == m.refd {
            return;
        }
        if !m.cleared {
            b.store_zero_at(m.at, lane, L_BIG_SLOT);
            m.cleared = true;
        }
        let zero = b.iconst(Type::I64, 0);
        let slot = b.load(Type::I64, lane, L_BIG_SLOT);
        let set = b.icmp(IntCond::Ne, slot, zero);
        let (l_set, l_back, l_on) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        b.mark_cold(l_set);
        b.mark_cold(l_back);
        b.cond_br(set, (l_set, &[]), (l_on, &[]));
        b.switch_to(l_set);
        b.store(lane, L_BIG_SLOT, zero);
        let mine = b.conv(ConvOp::PtrToInt, Type::I64, r);
        let mine = b.icmp(IntCond::Eq, slot, mine);
        b.cond_br(mine, (l_back, &[]), (l_on, &[]));
        b.switch_to(l_back);
        let mark = (b.load(Type::Ptr, lane, L_BIG_CUR), b.load(Type::Ptr, lane, L_BIG_END));
        self.give_back(b, mark);
        b.br(l_on, &[]);
        b.switch_to(l_on);
    }

    /// Whether the program has host sites (an entered function emits the cadence hooks).
    pub fn hosted(&self) -> bool {
        self.enter_h.is_some()
    }

    pub fn refs(&self, b: &mut FunctionBuilder<'_>, i: usize) -> (Option<Value>, Option<Value>) {
        let (Some(r), Some(arr)) = (self.slot, self.cells) else { return (None, None) };
        self.refd.set(self.refd.get() + 1);
        let c = b.load(Type::Ptr, arr, (i * 8) as i32);
        (Some(c), Some(r))
    }

    /// `row` with `ex`'s cells and region (`ex` found by its program's site buffer; none: a program without sites).
    pub fn with_ex<'a>(&self, b: &mut FunctionBuilder<'_>, ex: &Ex<'_>, row: RowRefs<'a>) -> RowRefs<'a> {
        let key = ex.c.prog.sites.as_ptr() as usize;
        match self.map.iter().find(|(k, _)| *k == key) {
            Some(&(_, i)) if !ex.c.prog.sites.is_empty() => self.with(b, i, row),
            _ => row,
        }
    }

    /// `row` with expression `i`'s cells and region.
    pub fn with<'a>(&self, b: &mut FunctionBuilder<'_>, i: usize, row: RowRefs<'a>) -> RowRefs<'a> {
        let (cells, region) = self.refs(b, i);
        if self.fmgr.get(i).copied().unwrap_or(false) {
            self.seen.borrow_mut().push(i); // alloc: Program
        }
        RowRefs { cells, region, ..row }
    }

    /// D3583: the mark `reset_seen` resets the windows of the expressions lowered after.
    pub fn seen(&self) -> usize {
        self.seen.borrow().len()
    }

    /// D3583: the function-manager windows of each expression lowered since `from` (`seen`) reset, PostgreSQL's per-tuple
    /// reset of an expanding step's row (FunctionScan / ProjectSet) and (D3591) of a nested loop's pair, beside the
    /// region's `give_back`. The step's set (the pairs of an outer row) runs inside one window of its input, so a site
    /// called per row of the set (`enum_range` in an `INSERT .. SELECT .. FROM generate_series`) kept every row's
    /// allocations until the window's reset (~17 kB a row each). Only the expressions lowered for the row after the mark:
    /// what was computed before (the input row's, the set's arguments, an outer row's NestLoopParam, a ProjectSet's held
    /// plain entries) keeps its window. What a later step or the end keeps past the row it copies already, as it does
    /// across windows (D2967).
    pub fn reset_seen(&self, b: &mut FunctionBuilder<'_>, from: usize, mcx: mcx::Mcx<'_>) {
        let Some(lane) = self.lane else { return };
        let mut xs = mcx::to_vec_in(&self.seen.borrow()[from..], mcx);
        xs.sort_unstable();
        xs.dedup();
        if xs.is_empty() {
            return;
        }
        let flags = HostFlags { touches_memory: true, ..HostFlags::default() };
        let h = b.declare_host_once("hosted_reset_ex", &[Type::Ptr, Type::I64], Some(Type::I64), raw::h_reset_ex as *const () as usize, flags);
        for i in xs {
            let iv = b.iconst(Type::I64, i as i64);
            b.call_host(h, &[lane, iv], Some(Type::I64));
        }
    }

    /// On a raise: the first site error of the row stashed for the entry (`gravity_fn::take_error`; the pool re-stashes a worker's on the leader).
    pub fn raised(&self, b: &mut FunctionBuilder<'_>) {
        let (Some(h), Some(lane)) = (self.raise_h, self.lane) else { return };
        b.call_host(h, &[lane], Some(Type::I64));
    }
}

unsafe fn lane_cells<'a>(l: &Lane) -> impl Iterator<Item = &'a mut Cell> {
    core::slice::from_raw_parts_mut(l.states.ptr(), l.n as usize).iter_mut().flat_map(|s| s.heads_mut())
}

/// Every lane past 0 gives each function-manager site a twin call and the lane a region of its own. M8 (D1036): a `nested`
/// lane (a run nested inside an outer row on the leader: an Init's members, a lazy build), so its resets never touch the
/// outer row's results. D2983: a pool lane, armed on its pool thread at the first entry and dropped with the program on the
/// leader (`FmgrCall::twin_pool`), so the thread calls its own frame and `fn_extra`.
/// planprod row 4: its lists are its own root's (`raw::owned_root`), owned at birth by the owner's holder: the statement for
/// a one-shot program, the session's cache once it filed the program (`adopt`), whatever thread arms it.
unsafe fn arm_lane(o: &Owner, l: &mut Lane, nested: bool) {
    let (root, m) = raw::owned_root("GravityHostedLane", o.holder.get());
    let mut calls = Vec::new(); // alloc: Program
    let mut states = reserved_in(o.hold.states.len(), m);
    // the owner's states include sites a pool lane never reaches (`pool_runs`); their twins are made and never called
    states.extend_in(o.hold.states.iter().map(|s| s.twin(&mut calls, !nested, m)));
    let mut cells = reserved_in(states.len(), m);
    cells.extend_in(states.iter_mut().map(|s| word(s.ptr())));
    let mut region = raw::region_in(BASE_BYTES, m);
    let (rb, re) = (region.as_mut_ptr().cast::<u8>(), region.as_mut_ptr_range().end.cast::<u8>());
    let mut hold = Box::new(LaneHold { states, cells, region, calls, root }); // alloc: Program
    *l = Lane { cells: word(hold.cells.as_ptr() as *mut Word<*mut Cell>), region: word(rb), region_end: word(re), states: word(hold.states.as_mut_ptr()), n: hold.states.len() as u64, hold: Some(hold), ctx: word(core::ptr::null_mut()), spills: NO_SPILLS, gen: 0, cat: gravity_deps::Opaque::NONE, track: NO_TRACK, big_slot: 0, big_cur: 0, big_end: 0 };
    spillable(re, l);
}

/// D641: the result regions `spill` may move, by their end (a lane's base region, or the spill chunk its slot is in): the lane.
/// D4073: a map, as every level of a deep SQL-function recursion registers its owner's lane and drops it (a scan of a list
/// per registration made the recursion O(depth^2)).
static SPILLABLE: pgsync::Mutex<BTreeMap<usize, usize>> = pgsync::Mutex::new(BTreeMap::new());

fn spillable(end: *mut u8, l: *mut Lane) {
    gravity_fn::set_spill_hook(spill);
    pgsync::lock(&SPILLABLE).insert(end as usize, l as usize); // alloc: Cold
}

/// D900: only lane `l`'s own entry at `end`: a region since re-allocated there (a chunk, a new lane's base) is another lane's.
fn unspillable(end: usize, l: usize) {
    let mut m = pgsync::lock(&SPILLABLE);
    if m.get(&end) == Some(&l) {
        m.remove(&end);
    }
}

fn spillable_lane(end: usize) -> Option<*mut Lane> {
    pgsync::lock(&SPILLABLE).get(&end).map(|&l| l as *mut Lane)
}

/// D641 (`Region::reserve`'s miss, never a hot path): a hosted function's result region `r` (its slot) moved to a spill
/// chunk with room for `n` more bytes. The bytes already written stay where they are (the base is never moved or freed), so a
/// result is copied once and no function runs twice. A slot back in the base (a rewind or release since its last spill) has
/// every byte of its chunks dead: they are free again first. A free chunk that fits is reused; else one is drawn from the
/// statement's grant, doubling the last from 16 bases (1 MiB) up to `SPILL_MAX`; F5 (memlead's A5 re-audit, R2): past the
/// grant pool the chunk is a session temp file mapped shared (`spill_host::v4::take`, D3021's road: temp_file_limit
/// charged, held to the statement's end), as PostgreSQL's one large result is its own palloc. A refusal (no file road, or
/// the file refused) leaves the region full (the named error).
unsafe fn spill(r: &mut gravity_fn::Region, n: usize) -> bool {
    let key = r.end.ptr() as usize;
    let Some(lane) = spillable_lane(key) else { return false };
    let l = &mut *lane;
    if l.ctx.ptr().is_null() {
        return false;
    }
    current(l);
    let ctx = l.ctx.ptr();
    let drawn = |bytes: usize| {
        let p = gravity_driver::mem::spill_host::v4::take(ctx, bytes);
        if !p.is_null() {
            spillable(p.add(bytes), lane);
        }
        p
    };
    // D3620: a lane with no base (`owner_of`'s `lazy`) draws its first chunk at the base's size, not 16 bases
    let floor = if l.region.ptr() == l.region_end.ptr() { BASE_BYTES } else { BASE_BYTES << 4 };
    let (cur0, end0) = (r.cur.ptr() as u64, r.end.ptr() as u64);
    match l.spills.take(r as *mut gravity_fn::Region as usize, r.end.ptr() == l.region_end.ptr(), n, floor, drawn) {
        Some((at, bytes)) => {
            fires::fire("hosted.spill"); // shapecheck fire signal (REGISTRY STEP-35, NEW-lowerv4-11, NEW-lowerv4-a2-126/127)
            r.cur = word(at as *mut u8);
            r.end = word((at + bytes) as *mut u8);
            // D3990: the row's first big result: an end that copies its row gives the bytes from here back (`Ir::give_back_big`)
            if n >= BIG_RESULT && l.big_slot == 0 {
                (l.big_slot, l.big_cur, l.big_end) = (r as *mut gravity_fn::Region as u64, cur0, end0);
            }
            true
        }
        None => false,
    }
}

impl Spills {
    /// A chunk for `slot` with room for `n` more bytes (its chunks free first when it is back in the base), reused or
    /// `draw`n (doubling the last draw from `floor` up to `SPILL_MAX`); `None` past `SPILL_MAX` or refused.
    fn take(&mut self, slot: usize, in_base: bool, n: usize, floor: usize, mut draw: impl FnMut(usize) -> *mut u8) -> Option<(usize, usize)> {
        if in_base {
            self.entered(slot);
        }
        let need = n.saturating_add(8);
        if need > SPILL_MAX {
            return None;
        }
        let i = match self.chunks.iter().position(|c| c.owner == 0 && c.bytes >= need) {
            Some(i) => i,
            None => {
                let bytes = need.max(floor.max(self.last.saturating_mul(2)).min(SPILL_MAX));
                let p = draw(bytes);
                if p.is_null() {
                    return None;
                }
                self.last = bytes;
                self.chunks.push(Chunk { at: p as usize, bytes, owner: 0 }); // alloc: Cold
                self.chunks.len() - 1
            }
        };
        let c = &mut self.chunks[i];
        c.owner = slot;
        Some((c.at, c.bytes))
    }

    /// `slot` back at the base (its function entered, or its region rewound): the chunks it wrote are free again.
    fn entered(&mut self, slot: usize) {
        self.chunks.iter_mut().filter(|c| c.owner == slot).for_each(|c| c.owner = 0);
    }
}

/// The lane's chunks forgotten when the driver has dropped them (the statement ended: its epoch moved) or they are
/// another Node's (the program re-entered on another ctx). A new statement after this thread's catalog views moved also
/// drops the twins' `fn_extra` (D3531, gravity_deps `Opaque`).
unsafe fn current(l: &mut Lane) {
    let e = gravity_driver::hostimpl::arena::spill_epoch(l.ctx.ptr());
    if e != l.spills.epoch {
        forget(l);
        l.spills.epoch = e;
        let cat = gravity_deps::Opaque::now();
        if cat != l.cat {
            lane_cells(l).filter(|c| !c.fmgr.ptr().is_null()).for_each(|c| (*c.fmgr.ptr()).reset_extra());
            l.cat = cat;
        }
    }
}

fn forget(l: &mut Lane) {
    let at = l as *mut Lane as usize;
    l.spills.chunks.drain(..).for_each(|c| unspillable(c.at + c.bytes, at));
    l.spills.last = 0;
}

impl Drop for Owner {
    fn drop(&mut self) {
        for l in self.lanes.each().filter(|l| !l.region_end.ptr().is_null()) {
            unspillable(l.region_end.ptr() as usize, l as *mut Lane as usize);
            forget(l);
        }
    }
}

/// What the program's owner keeps (`finish_keeping`'s `keep`), from `ProgramBlock::owner` at run time (V4R-101: a host
/// reaches its per-program state through the owner, never through an IR address that differs per lowering).
/// # Safety
/// `owner` is the running program's `ProgramBlock::owner`, of a program lowered with a `keep`.
pub(crate) unsafe fn kept<'a>(owner: *const u8) -> Option<&'a dyn Any> {
    (*(owner as *const Owner)).hold.keep.as_deref()
}

gravity_abi::host! {
    unsafe fn h_raise(lane: u64) -> u64 {
        for c in lane_cells(&*(lane as *const Lane)) {
            if let Some(e) = c.err.take() {
                gravity_fn::stash_error(e);
                break;
            }
        }
        0
    }
}

gravity_abi::host! {
    unsafe fn h_enter(owner: u64, ordinal: u64, ctx: u64, slot: u64) -> u64 {
        let o = &mut *(owner as *mut Owner);
        // D3862: worker 0 takes the lane its section's bind gave it (`host_lane0`: lane 0 at the top level, a free pool lane
        // inside a nested run), so a parallel member of a SubPlan run never rewinds the outer row's lane
        let ordinal = match ordinal {
            0 if ctx != 0 => (*(ctx as *const gravity_abi::Ctx)).host_lane0 as u64,
            n => n,
        };
        let l: *mut Lane = o.lanes.lane(ordinal as usize);
        if (*l).cells.ptr().is_null() {
            arm_lane(o, &mut *l, ordinal >= gravity_driver::NESTED_LANE_FIRST as u64);
        }
        // D641: the Call's ctx (another Node's chunks are forgotten); the entered function's slot starts at the base, so the
        // chunks it wrote are free again
        let lr = &mut *l;
        if lr.ctx.ptr() != ctx as *mut gravity_abi::Ctx {
            forget(lr);
            lr.ctx = word(ctx as *mut gravity_abi::Ctx);
        }
        current(lr);
        // B2: a lane past 0 entered under another claim generation than it last ran under drops every twin's `fn_extra`
        // (a memo built from a previous statement's armed catalog images); one compare per entry when unchanged
        let g = gravity_fn::session::claim_gen();
        if ordinal != 0 && lr.gen != g {
            lane_cells(lr).filter(|c| !c.fmgr.ptr().is_null()).for_each(|c| (*c.fmgr.ptr()).reset_extra());
            lr.gen = g;
        }
        lr.spills.entered(slot as usize);
        // each function-manager cell's window reset, and (D412) a cached program's function-usage bracket following the
        // session's track_functions, read once per entry: the cells retracked only when it moved since the lane's last
        // entry (rearm E4: a rescan enters its members' functions every outer row)
        let track = gravity_fn::FmgrCall::track_level();
        if lr.track != track {
            lr.track = track;
            lane_cells(&*l).filter(|c| !c.fmgr.ptr().is_null()).for_each(|c| (*c.fmgr.ptr()).retrack());
        }
        reset_windows(&*l);
        l as u64
    }
}

unsafe fn reset_windows(l: &Lane) {
    for c in lane_cells(l) {
        if !c.fmgr.ptr().is_null() {
            (*c.fmgr.ptr()).reset_window();
        }
    }
}

gravity_abi::host! {
    unsafe fn h_reset(lane: u64) -> u64 {
        reset_windows(&*(lane as *const Lane));
        0
    }
}

#[cfg(test)]
mod tests {
    use super::{Hosted, Spills, BASE_BYTES, NO_SPILLS, SPILL_MAX};

    /// D3620: an owner holds lane 0 alone until an entry past it, so a program only the leader runs (each deep statement
    /// level of a SQL-function recursion) does not hold the workers' and nested lanes' table (`LANES` lanes, ~30 KB); the
    /// first entry past lane 0 builds that table once, and no lane moves after it is built (its spill chunks' key).
    #[test]
    fn lanes_past_zero_are_built_at_their_first_entry() {
        let root = super::hosted_root();
        // SAFETY: `lanes` drops before `root` (declared after it)
        let mut lanes = super::Lanes::new(super::Lane::idle(), unsafe { root.handle() });
        assert_eq!(lanes.each().count(), 1, "a new owner holds lane 0 alone");
        let first = lanes.lane(0);
        assert!(lanes.rest.load(core::sync::atomic::Ordering::Acquire).is_null(), "lane 0's entries build no other lane");
        let three = lanes.lane(3);
        assert_eq!(lanes.each().count(), super::LANES, "the first entry past lane 0 builds the rest's table");
        assert_eq!((lanes.lane(0), lanes.lane(3)), (first, three), "a lane never moves");
        let top = lanes.lane(super::LANES - 1);
        // SAFETY: both lanes are in the rest's one table.
        assert_eq!(unsafe { top.offset_from(lanes.lane(1)) }, (super::LANES - 2) as isize, "one table of LANES - 1 lanes past lane 0");
    }

    /// D3620 race witness: workers entering fresh ordinals past 0 of one owner at once all take ONE table, the one the
    /// compare-exchange published; every other table a racer built is freed (built = losers' frees + 1).
    #[test]
    fn racing_first_entries_past_zero_keep_one_table() {
        struct Shared(*const super::Lanes);
        // SAFETY: the threads only call `rest_lane` (atomics) through it, and the lanes outlive the scope
        unsafe impl Sync for Shared {}
        const N: usize = 16;
        for _ in 0..50 {
            let root = super::hosted_root();
            // SAFETY: `lanes` drops before `root` (declared after it)
            let mut lanes = super::Lanes::new(super::Lane::idle(), unsafe { root.handle() });
            let shared = Shared(&lanes);
            let barrier = std::sync::Barrier::new(N);
            let bases: Vec<usize> = std::thread::scope(|s| {
                let hs: Vec<_> = (0..N).map(|i| {
                    let (shared, barrier) = (&shared, &barrier);
                    s.spawn(move || {
                        barrier.wait();
                        // SAFETY: see `Shared`
                        let l = unsafe { &*shared.0 };
                        let lane = l.rest_lane(1 + i % 7) as usize;
                        lane - (i % 7) * core::mem::size_of::<super::Lane>()
                    })
                }).collect();
                hs.into_iter().map(|h| h.join().unwrap()).collect()
            });
            assert!(bases.iter().all(|&b| b == bases[0]), "every racer took the same table");
            let (built, freed) = (lanes.tables.0.load(core::sync::atomic::Ordering::Relaxed), lanes.tables.1.load(core::sync::atomic::Ordering::Relaxed));
            assert_eq!(built, freed + 1, "one table kept, every loser's copy freed ({built} built)");
            assert_eq!(lanes.rest.load(core::sync::atomic::Ordering::Relaxed) as usize, bases[0], "the published table");
            assert_eq!(lanes.each().count(), super::LANES);
        }
    }

    /// D4073: one lane per region end (a later registration replaces the lane there; a lane drops only its own entry), and a
    /// deep recursion's levels register and drop without a scan per level.
    #[test]
    fn spillable_keeps_one_lane_per_end() {
        use super::{spillable, spillable_lane, unspillable, Lane};
        let (a, b, end) = (0x10 as *mut Lane, 0x20 as *mut Lane, 0x30usize);
        spillable(end as *mut u8, a);
        spillable(end as *mut u8, b);
        assert_eq!(spillable_lane(end), Some(b), "the later registration holds the end");
        unspillable(end, a as usize);
        assert_eq!(spillable_lane(end), Some(b), "a lane drops only its own entry");
        unspillable(end, b as usize);
        assert_eq!(spillable_lane(end), None);
        let at = |i: usize| 0x1000 + i * 0x40;
        let levels = 200_000;
        (0..levels).for_each(|i| spillable(at(i) as *mut u8, at(i) as *mut Lane));
        assert!((0..levels).all(|i| spillable_lane(at(i)) == Some(at(i) as *mut Lane)));
        (0..levels).rev().for_each(|i| unspillable(at(i), at(i)));
        assert!((0..levels).all(|i| spillable_lane(at(i)).is_none()));
    }

    /// D641: two functions of one program share lane 0. A spills and its datum lives on while B enters and spills: B never
    /// takes A's chunk, and A's bytes are intact. A's rewind frees its chunk for reuse; nothing is freed inside the statement.
    #[test]
    fn a_slot_never_takes_a_live_chunk_of_another() {
        let mut bufs: Vec<Vec<u8>> = Vec::new();
        let mut draw = |bytes: usize| {
            bufs.push(vec![0u8; bytes]);
            bufs.last_mut().unwrap().as_mut_ptr()
        };
        let mut s: Spills = NO_SPILLS;
        let (a, b) = (0x1000usize, 0x2000usize);
        let (xa, xn) = s.take(a, true, 1 << 19, BASE_BYTES << 4, &mut draw).unwrap();
        assert_eq!(xn, 1 << 20, "a first chunk of 16 bases");
        unsafe { core::ptr::write_bytes(xa as *mut u8, b'A', 1 << 19) };
        s.entered(b);
        let (ya, yn) = s.take(b, true, 1 << 19, BASE_BYTES << 4, &mut draw).unwrap();
        assert_ne!(ya, xa, "B took A's live chunk");
        assert_eq!(yn, 2 << 20, "the second draw doubles");
        unsafe { core::ptr::write_bytes(ya as *mut u8, b'B', 1 << 19) };
        assert!(unsafe { core::slice::from_raw_parts(xa as *const u8, 1 << 19) }.iter().all(|&c| c == b'A'), "A's datum was overwritten");
        // A's slot rewound to the base, then spills again: its own chunk is reused, B's stays B's
        let (za, _) = s.take(a, true, 1 << 19, BASE_BYTES << 4, &mut draw).unwrap();
        assert_eq!(za, xa);
        // A still in its chunk and needing more: a third chunk, the first two kept
        let (wa, _) = s.take(a, false, 1 << 19, BASE_BYTES << 4, &mut draw).unwrap();
        assert!(wa != xa && wa != ya);
        assert_eq!(s.chunks.len(), 3);
        assert!(s.take(a, false, 2 << 30, BASE_BYTES << 4, &mut draw).is_none(), "past MaxAllocSize");
    }

    /// STEP-35 (a capacity that grows): the base region is where a lane starts, not a bound: a result of any size up to
    /// PostgreSQL's MaxAllocSize takes a chunk that holds it (the draws double from 16 bases), and one past MaxAllocSize
    /// and the image's slack is declined, as palloc declines it.
    #[test]
    fn a_result_region_grows_to_max_alloc_size_and_no_further() {
        let mut drawn: Vec<usize> = Vec::new();
        let mut draw = |bytes: usize| {
            drawn.push(bytes);
            0x1000 as *mut u8
        };
        let mut s: Spills = NO_SPILLS;
        for n in [1, BASE_BYTES, BASE_BYTES << 4, (BASE_BYTES << 4) + 1, 100 << 20, mcx::MAX_ALLOC_SIZE] {
            let (_, bytes) = s.take(0x2000, false, n, BASE_BYTES << 4, &mut draw).expect("a result up to MaxAllocSize is taken");
            assert!(bytes > n, "{n}: a chunk of {bytes}");
        }
        assert!(s.take(0x2000, false, SPILL_MAX, BASE_BYTES << 4, &mut draw).is_none(), "past MaxAllocSize and the slack");
        assert!(drawn.windows(2).all(|w| w[1] >= w[0]), "a draw never shrinks: {drawn:?}");
    }

    /// D848: a hosted function's result region starts at 64 KiB, uninitialized (was 8 MiB zeroed per hosted function per
    /// lowering and per armed lane: under memory pressure each lowering faulted 4 huge pages through direct reclaim, D841).
    #[test]
    fn a_hosted_region_starts_small() {
        // planprod P4: the region is made in the owner's root (`owner_of`), `BASE_BYTES` when the lowering asks for a base
        const { assert!(BASE_BYTES <= 256 << 10) };
        let cx = mcx::MemoryContext::new("hir test");
        assert!(Hosted::build(&mut [], true, cx.mcx()).base, "a hosted lowering's lane 0 gets a base region");
        assert!(!Hosted::build(&mut [], false, cx.mcx()).base, "no hosted site: no region");
    }

    /// D3620: a lane with no base (a by-value-only owner's, `owner_of`'s `lazy`) draws its first chunk at the base's size,
    /// then doubles as any lane's draws do; a lane with a base starts at 16 bases.
    #[test]
    fn a_lane_without_a_base_draws_one_base_first() {
        let mut drawn: Vec<usize> = Vec::new();
        let mut draw = |bytes: usize| {
            drawn.push(bytes);
            0x1000 as *mut u8
        };
        let mut s: Spills = NO_SPILLS;
        let (_, first) = s.take(0x2000, true, 16, BASE_BYTES, &mut draw).expect("a chunk");
        assert_eq!(first, BASE_BYTES, "the first chunk of a lane without a base");
        let (_, second) = s.take(0x3000, false, 16, BASE_BYTES, &mut draw).expect("a chunk");
        assert_eq!(second, BASE_BYTES << 1, "then doubling");
        let mut t: Spills = NO_SPILLS;
        assert_eq!(t.take(0x2000, true, 16, BASE_BYTES << 4, &mut draw).expect("a chunk").1, BASE_BYTES << 4, "a based lane's first spill");
    }

    /// D3620: a by-value-only owner's lane 0 holds no base: an empty range at the lane's own address, registered as
    /// spillable while the owner lives (so a result that reaches it spills, never fails), gone with the owner.
    #[test]
    fn a_lazy_owner_registers_an_empty_base() {
        use gravity_abi::PtrWord;
        let root = super::hosted_root();
        let own = mcx::PgVec::new_in(super::raw::product_mcx(&root));
        let owner = super::owner_of(Vec::new(), super::Progs::Own(own, "test"), false, Vec::new(), None, &gravity_fn::pool_catalog::Reach::new_in(super::raw::product_mcx(&root)), true, Some(root));
        let o = owner.downcast_ref::<super::Owner>().expect("an owner");
        let at = &*o.lanes.first as *const super::Lane as usize;
        assert_eq!((o.lanes.first.region.ptr() as usize, o.lanes.first.region_end.ptr() as usize), (at, at), "an empty base at the lane");
        assert_eq!((o.region.ptr() as usize, o.region_end.ptr() as usize), (at, at), "the owner's lane 0 words as the lane's");
        assert_eq!(super::spillable_lane(at), Some(at as *mut super::Lane), "registered as spillable");
        drop(owner);
        assert_ne!(super::spillable_lane(at), Some(at as *mut super::Lane), "gone with the owner");
    }

    /// planprod P4: a hosted owner's product root (its base, its lists, and a program context under it, as `program_cx`
    /// makes) is the running statement's from birth, by construction; its holder's adoption (`adopt`) moves the whole tree
    /// to the session, and the owner's drop takes it off.
    #[test]
    fn a_hosted_owner_is_its_statements_until_a_holder_adopts_it() {
        std::thread::spawn(|| {
            // a procno inside the backend slots (BACKEND_SLOT_BASE + procno < ACCOUNT_SLOTS), or the thread is unbound
            let a = memledger::Acct::backend(4076);
            assert!(a.is_backend(), "a backend slot");
            memledger::bind_backend(4076, 9000 + 4076);
            memledger::statement_begin();
            let stmt = memledger::Owner::Statement { acct: a, stmt: memledger::running_statement(a) };
            let session = memledger::Owner::Session(a);
            let s0 = memledger::charge::owner_ctx(session).0;
            let root = super::hosted_root();
            let child = Box::new(root.new_child("GravityHostedTestCx"));
            // a node list the program's compiles read, freed with the context
            core::mem::forget(mcx::reserved_in::<u8>(16 << 10, child.mcx()));
            let keep = Some(Box::new(vec![child]) as Box<dyn core::any::Any>);
            let own = mcx::PgVec::new_in(super::raw::product_mcx(&root));
            let owner = super::owner_of(Vec::new(), super::Progs::Own(own, "test"), true, Vec::new(), keep, &gravity_fn::pool_catalog::Reach::new_in(super::raw::product_mcx(&root)), false, Some(root));
            let built = memledger::charge::owner_ctx(stmt).0;
            assert!(built >= (super::BASE_BYTES + (16 << 10)) as i64, "the base and the child's list are the statement's: {built}");
            super::adopt(&*owner, session);
            assert_eq!(memledger::charge::owner_ctx(stmt).0, 0, "adopted whole: nothing left on the statement");
            assert!(memledger::charge::owner_ctx(session).0 - s0 >= built, "the session holds the tree");
            drop(owner);
            assert_eq!(memledger::charge::owner_ctx(session).0, s0, "and the owner's drop takes it off");
            memledger::statement_end();
            memledger::unbind_backend();
        })
        .join()
        .unwrap();
    }

    /// A pool worker's first entry arming lane `ordinal` of owner `o` (`h_enter`'s road), on a thread of its own.
    fn arm_on_worker(o: usize, ordinal: usize) {
        std::thread::spawn(move || {
            // SAFETY: the owner outlives the join below, and no other entry runs it
            unsafe {
                let o = &*(o as *const super::Owner);
                let l = o.lanes.rest_lane(ordinal);
                super::arm_lane(o, &mut *l, false);
            }
        })
        .join()
        .unwrap();
    }

    /// planprod row 4: a pool lane armed on a worker under a cached program (its owner adopted by the session, as the
    /// backend's cache files it) is the session's from its birth, whatever thread arms it; it outlives the statement with
    /// the program, and dropping the program on the leader takes the lane's root off the session.
    #[test]
    fn a_cached_programs_pool_lane_is_the_sessions_until_the_program_drops() {
        std::thread::spawn(|| {
            let a = memledger::Acct::backend(4075);
            assert!(a.is_backend(), "a backend slot");
            memledger::bind_backend(4075, 9000 + 4075);
            memledger::statement_begin();
            let stmt = memledger::Owner::Statement { acct: a, stmt: memledger::running_statement(a) };
            let session = memledger::Owner::Session(a);
            let s0 = memledger::charge::owner_ctx(session).0;
            let root = super::hosted_root();
            let own = mcx::PgVec::new_in(super::raw::product_mcx(&root));
            let owner = super::owner_of(Vec::new(), super::Progs::Own(own, "test"), false, Vec::new(), None, &gravity_fn::pool_catalog::Reach::new_in(super::raw::product_mcx(&root)), true, Some(root));
            super::adopt(&*owner, session);
            let filed = memledger::charge::owner_ctx(session).0;
            arm_on_worker(owner.downcast_ref::<super::Owner>().unwrap() as *const super::Owner as usize, 1);
            let armed = memledger::charge::owner_ctx(session).0;
            eprintln!("row 4 witness: a pool lane of a cached program: session {s0} -> filed {filed} -> armed {armed} ({} bytes)", armed - filed);
            assert!(armed - filed >= super::BASE_BYTES as i64, "the lane's root is the session's: {filed} -> {armed}");
            assert_eq!(memledger::charge::owner_ctx(stmt).0, 0, "nothing on the statement");
            memledger::statement_end();
            assert_eq!(memledger::charge::owner_ctx(session).0, armed, "the lane outlives the statement with the program");
            drop(owner);
            assert_eq!(memledger::charge::owner_ctx(session).0, s0, "dropping the program on the leader takes the lane off");
            memledger::unbind_backend();
        })
        .join()
        .unwrap();
    }

    /// planprod row 4: a one-shot program's lanes (its owner never adopted), a pool worker's and a nested one on the
    /// leader, are the statement's, and leave with the program at the statement's end. A lane armed before the adoption
    /// moves with it (`Lanes::reown`).
    #[test]
    fn a_one_shot_programs_lanes_are_the_statements_and_move_with_an_adoption() {
        std::thread::spawn(|| {
            let a = memledger::Acct::backend(4074);
            assert!(a.is_backend(), "a backend slot");
            memledger::bind_backend(4074, 9000 + 4074);
            memledger::statement_begin();
            let stmt = memledger::Owner::Statement { acct: a, stmt: memledger::running_statement(a) };
            let session = memledger::Owner::Session(a);
            let root = super::hosted_root();
            let own = mcx::PgVec::new_in(super::raw::product_mcx(&root));
            let owner = super::owner_of(Vec::new(), super::Progs::Own(own, "test"), false, Vec::new(), None, &gravity_fn::pool_catalog::Reach::new_in(super::raw::product_mcx(&root)), true, Some(root));
            let built = memledger::charge::owner_ctx(stmt).0;
            let o = owner.downcast_ref::<super::Owner>().unwrap() as *const super::Owner as usize;
            arm_on_worker(o, 1);
            // SAFETY: the owner is live and nothing else enters it
            unsafe {
                let o = &*(o as *const super::Owner);
                let l = o.lanes.rest_lane(gravity_driver::NESTED_LANE_FIRST as usize);
                super::arm_lane(o, &mut *l, true);
            }
            let armed = memledger::charge::owner_ctx(stmt).0;
            eprintln!("row 4 witness: a one-shot program's two lanes: statement {built} -> {armed} ({} bytes)", armed - built);
            assert!(armed - built >= 2 * super::BASE_BYTES as i64, "both lanes are the statement's: {built} -> {armed}");
            let s0 = memledger::charge::owner_ctx(session).0;
            super::adopt(&*owner, session);
            assert_eq!(memledger::charge::owner_ctx(stmt).0, 0, "an adoption moves the armed lanes too");
            assert_eq!(memledger::charge::owner_ctx(session).0 - s0, armed, "the session holds the owner and its lanes");
            drop(owner);
            assert_eq!(memledger::charge::owner_ctx(session).0, s0, "gone with the program");
            memledger::statement_end();
            memledger::unbind_backend();
        })
        .join()
        .unwrap();
    }

    /// planprod row 4: a one-shot program's lane leaves the statement with the program, at the statement's end.
    #[test]
    fn a_one_shot_programs_lane_leaves_with_the_program() {
        std::thread::spawn(|| {
            let a = memledger::Acct::backend(4073);
            assert!(a.is_backend(), "a backend slot");
            memledger::bind_backend(4073, 9000 + 4073);
            memledger::statement_begin();
            let stmt = memledger::Owner::Statement { acct: a, stmt: memledger::running_statement(a) };
            let root = super::hosted_root();
            let own = mcx::PgVec::new_in(super::raw::product_mcx(&root));
            let owner = super::owner_of(Vec::new(), super::Progs::Own(own, "test"), false, Vec::new(), None, &gravity_fn::pool_catalog::Reach::new_in(super::raw::product_mcx(&root)), true, Some(root));
            arm_on_worker(owner.downcast_ref::<super::Owner>().unwrap() as *const super::Owner as usize, 2);
            assert!(memledger::charge::owner_ctx(stmt).0 >= super::BASE_BYTES as i64, "the lane is the statement's");
            drop(owner);
            assert_eq!(memledger::charge::owner_ctx(stmt).0, 0, "gone with the program");
            memledger::statement_end();
            memledger::unbind_backend();
        })
        .join()
        .unwrap();
    }

    /// A template (`make_template`) over one program whose lists hold `oids` words: the root holds them. memsig E-prog: the
    /// lists are built in place in a lowering's lists root (frame `Px::lists_mcx`), compacted into the template's root at
    /// the filing, and the lists root drops after (`finish_keeping`).
    fn tpl_of(oids: usize) -> super::Progs {
        let mut lists = mcx::SendRoot::new_owned_small("GravityHostedLists", memledger::charge::statement_owner());
        let mut prog = gravity_expr::Program::empty_in(super::raw::root_mcx(&mut lists));
        mcx::GrowIn::resize_in(&mut prog.oids, oids, 7);
        let c = gravity_expr::Compiled { prog, state: gravity_expr::State::empty(), root: gravity_expr::NodeId(0), params: types_portal::params::ParamBind::NONE, callback_arg: core::ptr::null_mut(), result_type: 0 };
        let t = super::make_template(vec![c], &mut [], Vec::new(), Vec::new(), true, true, &gravity_fn::pool_catalog::Reach::new_in(mcx::MemoryContext::new("reach test").mcx()));
        drop(lists);
        t
    }

    /// planprod P6: a template's lists are its root's, the lowering statement's at birth; the building backend's
    /// adoption makes them the session's (with the owner's own root), the tier's publish the program caches' (here a stand-in
    /// holder no other test of the crate charges, so the count is exact under concurrent tests; `publish` passes
    /// `PROGRAM_CACHES`),
    /// once: no adoption moves them after. They leave that owner when the last holder drops the template, on another thread.
    #[test]
    fn a_templates_root_is_the_statements_then_the_sessions_then_the_tiers_until_its_last_drop() {
        std::thread::spawn(|| {
            let a = memledger::Acct::backend(4072);
            let tier = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::FpTables));
            memledger::bind_backend(4072, 9000 + 4072);
            memledger::statement_begin();
            let stmt = memledger::Owner::Statement { acct: a, stmt: memledger::running_statement(a) };
            let session = memledger::Owner::Session(a);
            let (s0, c0) = (memledger::charge::owner_ctx(session).0, memledger::charge::owner_ctx(tier).0);
            let progs = tpl_of(16 << 10);
            let born = memledger::charge::owner_ctx(stmt).0;
            let owner = super::owner_of(Vec::new(), progs, false, Vec::new(), None, &gravity_fn::pool_catalog::Reach::new_in(mcx::MemoryContext::new("reach test").mcx()), true, None);
            let t = super::template(&*owner).expect("a template owner");
            let tpl = t.downcast_ref::<super::raw::HostTpl>().unwrap();
            let bytes = tpl.root_bytes() as i64;
            assert!(born >= bytes && bytes >= 64 << 10, "the root holds the lists, the statement's: {born} {bytes}");
            super::adopt(&*owner, session);
            let filed = memledger::charge::owner_ctx(session).0;
            assert_eq!(memledger::charge::owner_ctx(stmt).0, 0, "the adoption moves the template's root too");
            assert!(filed - s0 >= bytes, "the session holds the template: {s0} -> {filed}");
            let k0 = memledger::charge::owner_row(tier).committed;
            tpl.publish(tier);
            let own = tpl.own_bytes();
            assert!(own >= memledger::arc_bytes::<super::raw::HostTpl>() + mcx::SendRoot::HEADER_BYTES, "D4580: its own blocks: {own}");
            assert_eq!(memledger::charge::owner_row(tier).committed - k0, own as i64, "D4580: and its own blocks, on a charge of the tier's");
            let published = memledger::charge::owner_ctx(session).0;
            eprintln!("P6 witness: a template's root of {bytes} bytes: statement {born} -> session {s0} -> {filed} -> published {published}, tier {c0} -> {}", memledger::charge::owner_ctx(tier).0);
            assert_eq!(filed - published, bytes, "the publish takes exactly the root off the session");
            assert_eq!(memledger::charge::owner_ctx(tier).0 - c0, bytes, "onto the tier");
            super::adopt(&*owner, session);
            assert_eq!(memledger::charge::owner_ctx(session).0, published, "never moves after the publish");
            drop(owner);
            assert_eq!(memledger::charge::owner_ctx(session).0, s0, "the owner's own root leaves with it");
            assert_eq!(memledger::charge::owner_ctx(tier).0 - c0, bytes, "the template stays while a holder keeps it");
            memledger::statement_end();
            std::thread::spawn(move || drop(t)).join().unwrap();
            assert_eq!(memledger::charge::owner_ctx(tier).0, c0, "the last drop, on another thread, takes it off");
            assert_eq!(memledger::charge::owner_row(tier).committed, k0, "its own blocks too");
            memledger::unbind_backend();
        })
        .join()
        .unwrap();
    }

    /// planprod P6: a template the tier does not take stays on the building session and leaves with its owner; another
    /// owner rebuilt from it (`rebuild`) shares it and moves none of it at its own adoption.
    #[test]
    fn a_template_the_tier_refuses_stays_the_sessions_and_leaves_with_the_owner() {
        std::thread::spawn(|| {
            let a = memledger::Acct::backend(4070);
            memledger::bind_backend(4070, 9000 + 4070);
            memledger::statement_begin();
            let session = memledger::Owner::Session(a);
            let s0 = memledger::charge::owner_ctx(session).0;
            let owner = super::owner_of(Vec::new(), tpl_of(16 << 10), false, Vec::new(), None, &gravity_fn::pool_catalog::Reach::new_in(mcx::MemoryContext::new("reach test").mcx()), true, None);
            let t = super::template(&*owner).expect("a template owner");
            let bytes = t.downcast_ref::<super::raw::HostTpl>().unwrap().root_bytes() as i64;
            super::adopt(&*owner, session);
            let filed = memledger::charge::owner_ctx(session).0;
            assert!(filed - s0 >= bytes, "the session holds the template: {s0} -> {filed}");
            let rebuilt = super::rebuild(&t).expect("no stamp, no call: it rebuilds");
            let other = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::ZoneStore));
            let o0 = memledger::charge::owner_ctx(other).0;
            super::adopt(&*rebuilt, other);
            let o1 = memledger::charge::owner_ctx(other).0;
            assert_eq!(memledger::charge::owner_ctx(session).0, filed, "a rebuilt owner's adoption moves none of the template");
            assert!(o1 - o0 < bytes, "only the rebuilt owner's own root: {o0} -> {o1}");
            drop(rebuilt);
            drop(t);
            drop(owner);
            eprintln!("P6 witness: a refused template of {bytes} bytes: session {s0} -> {filed} -> {}", memledger::charge::owner_ctx(session).0);
            assert_eq!(memledger::charge::owner_ctx(session).0, s0, "gone with the owner");
            assert_eq!(memledger::charge::owner_ctx(other).0, o0, "and the rebuilt one's root with it");
            memledger::statement_end();
            memledger::unbind_backend();
        })
        .join()
        .unwrap();
    }
    /// memsig E-prog witness: the programs' lists, built in place in a lowering's lists root (frame `Px::lists_mcx`) and
    /// compacted into the template's root at the filing (the lists root gone), are the statement's while it lowers and 0
    /// after its end, the session's once filed, the tier's once published; a second session's owner rebuilt from the
    /// published template holds none of them; the eviction (the last drop, on another thread) leaves every account where
    /// it started. The tier is a stand-in holder no other test of the crate charges (`CondCache`), so its counts are exact
    /// under concurrent tests.
    #[test]
    fn a_lowerings_lists_compact_into_the_template_through_a_second_session_and_the_eviction() {
        std::thread::spawn(|| {
            let a = memledger::Acct::backend(4078);
            let tier = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::CondCache));
            memledger::bind_backend(4078, 9000 + 4078);
            memledger::statement_begin();
            let stmt = memledger::Owner::Statement { acct: a, stmt: memledger::running_statement(a) };
            let session = memledger::Owner::Session(a);
            let (s0, c0) = (memledger::charge::owner_ctx(session).0, memledger::charge::owner_ctx(tier).0);
            let owner = super::owner_of(Vec::new(), tpl_of(16 << 10), false, Vec::new(), None, &gravity_fn::pool_catalog::Reach::new_in(mcx::MemoryContext::new("reach test").mcx()), true, None);
            let t = super::template(&*owner).expect("a template owner");
            let bytes = t.downcast_ref::<super::raw::HostTpl>().unwrap().root_bytes() as i64;
            let born = memledger::charge::owner_ctx(stmt).0;
            assert!(born >= bytes && bytes >= 64 << 10, "the template's root holds the compacted lists, the statement's: {born} {bytes}");
            super::adopt(&*owner, session);
            let filed = memledger::charge::owner_ctx(session).0;
            assert!(filed - s0 >= bytes, "the session holds the template: {s0} -> {filed}");
            t.downcast_ref::<super::raw::HostTpl>().unwrap().publish(tier);
            assert_eq!(memledger::charge::owner_ctx(tier).0 - c0, bytes, "the tier holds the template's root");
            memledger::statement_end();
            assert_eq!(memledger::charge::owner_ctx(stmt).0, 0, "the statement holds nothing after its end");
            // a second session rebuilds an owner from the published template: its own root only
            let t2 = t.clone();
            let second = std::thread::spawn(move || {
                let b = memledger::Acct::backend(4077);
                memledger::bind_backend(4077, 9000 + 4077);
                memledger::statement_begin();
                let session2 = memledger::Owner::Session(b);
                let z0 = memledger::charge::owner_ctx(session2).0;
                let rebuilt = super::rebuild(&t2).expect("no stamp, no call: it rebuilds");
                super::adopt(&*rebuilt, session2);
                let z1 = memledger::charge::owner_ctx(session2).0;
                drop(rebuilt);
                drop(t2);
                memledger::statement_end();
                let z2 = memledger::charge::owner_ctx(session2).0;
                memledger::unbind_backend();
                (z0, z1, z2)
            });
            let (z0, z1, z2) = second.join().unwrap();
            assert!(z1 - z0 < bytes, "the second session holds only its owner's own root: {z0} -> {z1}");
            assert_eq!(z2, z0, "and none of it after its owner drops");
            assert_eq!(memledger::charge::owner_ctx(tier).0 - c0, bytes, "the template stays the tier's while a holder keeps it");
            drop(owner);
            assert_eq!(memledger::charge::owner_ctx(session).0, s0, "the first session is where it started");
            std::thread::spawn(move || drop(t)).join().unwrap();
            eprintln!("E-prog witness: a template root of {bytes} bytes: statement {born} -> 0, session {s0} -> {filed} -> {}, second session {z0} -> {z1} -> {z2}, tier {c0} -> {}", memledger::charge::owner_ctx(session).0, memledger::charge::owner_ctx(tier).0);
            assert_eq!(memledger::charge::owner_ctx(tier).0, c0, "the eviction leaves the tier where it started");
            memledger::unbind_backend();
        })
        .join()
        .unwrap();
    }
}

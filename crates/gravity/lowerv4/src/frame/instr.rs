//! EXPLAIN ANALYZE's per-node counters (lowerv4 plan §2.1, P6; ENTRY E1 at c79e936b690; v1 explain-text.md §1): a lowering
//! under `scope(true, ..)` allocates one counter cell per (plan node, counter) and adds into the program block's counter
//! region; an uninstrumented lowering emits no IR at all. A program's pipelines count their plan nodes at the node
//! boundaries of their chains (`Bounds`, made by `resolve`): a row entering a step of another node counts one row of every
//! node it left, a row reaching the end one of every node still open; each pipeline counts one `Loops` of the nodes it
//! starts. A node no pipeline runs in its order (a swapped build side, a drain fused into another pipeline, a spilled pass)
//! has no cell and prints as not counted. A mechanism counts what only it sees (`Filtered1` / `Filtered2`).
use core::cell::{Cell, RefCell};

use gravity_abi::ctx::hdr;
use gravity_abi::program::prog;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::promote::{Cell as Carried, Root};
use gravity_ir::{CloneIn, Func, Function, IntBinOp, IntCond, IntUnOp, Op, StateMask, Type, Value};
use mcx::{CollectIn, GrowIn, Mcx, PgVec};

/// What a cell counts for its plan node (the entry's `Instrumentation` fields).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Ctr {
    /// `ntuples`: where its rows leave it, the op boundary after its last op.
    Rows = 0,
    /// `nloops`: per open of its source, per rescan, per outer row of a re-run inner.
    Loops = 1,
    /// `nfiltered1`: rows its own qual (a join's join qual) removed.
    Filtered1 = 2,
    /// `nfiltered2`: rows a join node's own qual removed.
    Filtered2 = 3,
    /// No count: the run cut the node's rows short of PostgreSQL's (a cell nothing adds to; the node prints as not
    /// counted).
    Partial = 4,
    /// No count: the node's rows are the rows of the node the next cell (`SameAs`) names, the first node of the one
    /// boundary that counts both (`seal_plan`).
    Same = 5,
    /// The node a `Same` cell takes its rows from (a cell nothing adds to).
    SameAs = 6,
    /// No count: the node's rows are counted by the program pipeline the next cell (`PipeOf`) names (`pipe_order`), so
    /// EXPLAIN ANALYZE reads the node's time from that pipeline's run (a cell nothing adds to).
    Pipe = 7,
    /// The program pipeline (its index, as `node`) a `Pipe` cell names (a cell nothing adds to).
    PipeOf = 8,
    /// A Memoize's cache counters (PostgreSQL's MemoizeInstrumentation: hits, misses, evictions, overflows, peak bytes),
    /// five cells in that order the program never adds to: the run's end writes them from the memo object the `MemoOf`
    /// cell after them names (exec `State::fill_cells`).
    Hits = 9,
    Misses = 10,
    Evictions = 11,
    Overflows = 12,
    MemPeak = 13,
    /// The memo object's handle word (its index, as `node`) the five cells before it read (a cell nothing adds to).
    MemoOf = 14,
    /// A MERGE's rows its actions wrote, on its ModifyTable node (PostgreSQL's mt_merge_inserted / mt_merge_updated /
    /// mt_merge_deleted): one where an action's write took its row.
    MergeInserted = 15,
    MergeUpdated = 16,
    MergeDeleted = 17,
    /// `ntuples2` of an INSERT ... ON CONFLICT's ModifyTable node (PostgreSQL's InstrCountTuples2 in ExecInsert): one per
    /// proposed row that met a conflict, DO NOTHING's skipped row or DO UPDATE's row the conflict arm took.
    Tuples2 = 18,
    /// An Incremental Sort's IncrementalSortInfo (Full-sort, then Pre-sorted: groups, sort method bits, total and peak
    /// kB), then the bytes of work_mem its replay needed and whether the rows read were not PostgreSQL's: ten cells its
    /// top's finish adds from the replay (ops/sort/inc_replay.rs, in `Agg` order).
    IncFullGroups = 19,
    IncFullMethods = 20,
    IncFullTotal = 21,
    IncFullPeak = 22,
    IncPreGroups = 23,
    IncPreMethods = 24,
    IncPreTotal = 25,
    IncPrePeak = 26,
    IncNeed = 27,
    IncBad = 28,
    /// D3500: the rows a MERGE's actions took, on its ModifyTable node (one per row reaching its write). Only a recheck
    /// node's count is read (driver node/raw.rs `fold_recheck`): the rows a write's recheck handed the actions in place of
    /// the one row it rechecks.
    MergeTaken = 29,
    /// D3500: the rows a MERGE's rechecks handed its actions beyond the one row each rechecks, on its ModifyTable node: a
    /// MATCHED row whose recheck no longer joins, under both BY SOURCE and NOT MATCHED actions, goes on as the target row
    /// and the source row, which PostgreSQL's ExecMergeMatched counts on the outer plan (InstrUpdateTupleCount). A cell the
    /// program never adds to: the driver's `fold_recheck` does, and exec instrument.rs adds it to the outer plan's rows.
    MergeSplit = 30,
    /// The InitPlan whose plan id the cell's `node` holds is quiet (decompose F5: it calls nothing VOLATILE and cannot
    /// raise), so a run of it ahead of PostgreSQL's first read of its parameter shows nothing but its own counts: EXPLAIN
    /// ANALYZE may print it never executed where no node reading its parameters evaluated one (exec instrument.rs
    /// `init_ran`). A cell nothing adds to.
    InitQuiet = 40,
    /// nest.selfspool (D3814): the node is a synthetic Materialize's fill and emit (decompose self_spool.rs `wrap`, no plan
    /// node of its own), which PostgreSQL rescans per outer row: only its reader counts it (a loop per re-open, the rows it
    /// hands on), and its quals ran once, at the fill, so EXPLAIN ANALYZE prints no Rows Removed for it (exec instrument.rs).
    /// A cell nothing adds to.
    SpoolRead = 41,
}

/// EXPLAIN ANALYZE: node `t` is a synthetic Materialize's fill and emit (`Ctr::SpoolRead`): its reader alone counts it
/// (`seal_plan`); nothing when the lowering is not instrumented.
pub fn reader_owned(t: i32) {
    if on() {
        let _ = cell(t, Ctr::SpoolRead);
    }
}

/// Node `n` is counted by its reader alone (`reader_owned`).
fn reader_only(n: i32) -> bool {
    CELLS.with(|c| c.borrow().iter().any(|x| x.node == n && x.ctr == Ctr::SpoolRead))
}

/// EXPLAIN ANALYZE: the quiet InitPlan `plan_id`'s marker cell (`Ctr::InitQuiet`); nothing when the lowering is not
/// instrumented.
pub fn init_quiet(plan_id: i32) {
    if on() {
        let _ = cell(plan_id, Ctr::InitQuiet);
    }
}

/// EXPLAIN ANALYZE: MERGE node `mt`'s recheck cell (`Ctr::MergeSplit`); nothing when the lowering is not instrumented.
pub fn merge_split(mt: i32) {
    if on() {
        let _ = cell(mt, Ctr::MergeSplit);
    }
}

/// The Incremental Sort cells in the replay's word order (`Ctr::IncFullGroups` .. `Ctr::IncBad`).
pub const INC_STATS: [Ctr; 10] = [
    Ctr::IncFullGroups,
    Ctr::IncFullMethods,
    Ctr::IncFullTotal,
    Ctr::IncFullPeak,
    Ctr::IncPreGroups,
    Ctr::IncPreMethods,
    Ctr::IncPreTotal,
    Ctr::IncPrePeak,
    Ctr::IncNeed,
    Ctr::IncBad,
];

/// EXPLAIN ANALYZE: Memoize `node`'s cache counter cells over the memo object in handle word `word` (`Ctr::Hits` ..
/// `Ctr::MemoOf`, six cells in a row); nothing when the lowering is not instrumented or they are there.
pub fn memo_cells(node: i32, word: u16) {
    if !on() {
        return;
    }
    CELLS.with(|c| {
        let mut c = c.borrow_mut();
        if c.iter().any(|x| x.node == node && x.ctr == Ctr::Hits) {
            return;
        }
        for ctr in [Ctr::Hits, Ctr::Misses, Ctr::Evictions, Ctr::Overflows, Ctr::MemPeak] {
            c.push(InstrCell { node, ctr }); // alloc: Program
        }
        c.push(InstrCell { node: word as i32, ctr: Ctr::MemoOf }); // alloc: Program
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InstrCell {
    pub node: i32,
    pub ctr: Ctr,
}

/// One pipeline's node boundaries: `at[j]` the nodes a row entering step `j` (op `j`) has left, `end` the nodes a row
/// leaving the pipeline's ops (the gate's step, a tail's first, or the end) has left, `loops` the nodes whose starts the
/// pipeline's open counts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bounds {
    pub at: Vec<Vec<i32>>,
    pub end: Vec<i32>,
    pub loops: Vec<i32>,
    /// The end's rows are its write-backs' `out` (`EndMech::out_counts_rows`, `by_out`): no row adds to `end`'s cells.
    pub out: bool,
    /// `at[0]`'s rows are the window's positions (`by_window`): counted once per window, the rest a stop leaves unread
    /// taken back at its write-back (`uncount_ahead`).
    pub win: bool,
    /// The source's node (the entry owning op 0): its own quals' failures count as its `Filtered1` where they fail
    /// (`filters_counted`), PostgreSQL's Rows Removed by Filter of a scan.
    pub src: Option<i32>,
}

impl Bounds {
    /// The boundaries of a pipeline of `nops` ops whose plan nodes are `entries` (`(node, first_op, producer_top)`, source
    /// first: decompose's `node_ops`); `None` when they do not name its ops in order. A node owns the ops from its first to
    /// the next node's first, so its rows are the rows entering the next node's first op. A breaker's entry (producer_top)
    /// counts no rows here, its rows being its emit's, except a hash build's (`hash`: the rows the Hash node inserts);
    /// `own` excludes a node that counts its own rows (a write's).
    pub fn of(entries: &[(i32, u16, bool)], nops: usize, hash: bool, own: &dyn Fn(i32) -> bool) -> Option<Bounds> {
        if entries.is_empty() || entries.windows(2).any(|w| w[0].1 > w[1].1) || entries.iter().any(|e| e.1 as usize > nops) {
            return None;
        }
        // the node owning op 0: the last of the entries starting there (a bitmap heap scan after its index scans, which
        // own no op)
        let src = entries.iter().take_while(|e| e.1 == 0).last().filter(|e| !e.2).map(|e| e.0);
        let mut b = Bounds { at: vec![Vec::new(); nops], end: Vec::new(), loops: Vec::new(), out: false, win: false, src }; // alloc: Program
        for (i, &(node, _, top)) in entries.iter().enumerate() {
            if (top && !hash) || own(node) || b.loops.contains(&node) {
                continue;
            }
            let ends = entries.get(i + 1).map_or(nops, |n| n.1 as usize);
            match b.at.get_mut(ends) {
                Some(at) => at.push(node), // alloc: Program
                None => b.end.push(node),  // alloc: Program
            }
            b.loops.push(node); // alloc: Program
        }
        Some(b)
    }
}

/// The instrumented program's plan: each graph pipeline's boundaries, the nodes counted by boundaries, the nodes no
/// boundary may count.
#[derive(Default)]
struct Plan {
    active: bool,
    pipes: Vec<(usize, Bounds)>,
    unsafe_nodes: Vec<i32>,
    counted: Vec<i32>,
    /// (graph pipeline, probe op) of each join whose runtime filter may cut the pipeline's scan (`cut`).
    cuts: Vec<(usize, usize)>,
    /// (node, the node whose rows it takes) for each boundary's later nodes (`seal_plan`, `Ctr::Same`).
    alias: Vec<(i32, i32)>,
    /// (graph pipeline, node) for each node a pipeline's boundaries count (`seal_plan`), and (node, program pipeline) once
    /// the program's order is known (`pipe_order`): the `Pipe` / `PipeOf` cells.
    members: Vec<(usize, i32)>,
    pipes_of: Vec<(i32, u16)>,
    /// (a nest's embedded inner pipeline, the pipeline that runs it): an inner's nodes take their owner's time.
    inners: Vec<(usize, usize)>,
}

leader_tls::leader_tls! {
    thread_local! {
        static ON: Cell<bool> = const { Cell::new(false) };
        static CELLS: RefCell<Vec<InstrCell>> = const { RefCell::new(Vec::new()) };
        static PLAN: RefCell<Plan> = RefCell::new(Plan::default());
        static CURRENT: RefCell<Option<Bounds>> = const { RefCell::new(None) };
        static BASE: Cell<Option<(usize, Value)>> = const { Cell::new(None) };
        /// The current function's word holding the end of the window whose rows `at[0]` counted ahead (`by_window`).
        static AHEAD: Cell<Option<(usize, Value)>> = const { Cell::new(None) };
        /// The current function's counter offsets a row adds to (`count_row`, `count_one`): the ones `carry` keeps in registers.
        static HOT: RefCell<Vec<i32>> = const { RefCell::new(Vec::new()) };
        static BASES: RefCell<Vec<(Func, Value, Vec<i32>)>> = const { RefCell::new(Vec::new()) };
    }
}

/// The class the register pass carries a counter in that more than one function touches: no host names it, so only a
/// host that may touch any state (`StateMask::ALL`) and an intra-program call write it back (and reload it after).
const COUNTERS: StateMask = StateMask(0x40);

/// The class of a counter one function alone touches (`cell_users`). No host reads or writes the counter region (the
/// driver reads it once a function has returned: a pipeline's end, a section's close), so `carry` makes every host
/// transparent to this class; only a return and a call into another function of the program write it back.
const OWN: StateMask = StateMask(0x80);

/// A planned frame's function loads its counter region's base once, at its entry (`b` there): every count the function
/// makes goes through it, so the register pass (`carry`) can keep the counters in registers.
pub fn open_base(b: &mut FunctionBuilder<'_>, ctx: Value) {
    if !on() || !planned() {
        return;
    }
    let block = b.load(Type::Ptr, ctx, hdr::PROGRAM as i32);
    let base = b.load(Type::Ptr, block, prog::INSTR as i32);
    BASE.with(|x| x.set(Some((b.func() as *const Function as usize, base))));
    HOT.with(|h| h.borrow_mut().clear());
}

/// The end of function `f` (`open_base`'s): its base is the one `carry` promotes through.
pub fn close_base(f: Func) {
    if let Some((_, v)) = BASE.with(Cell::take) {
        let hot = HOT.with(|h| core::mem::take(&mut *h.borrow_mut()));
        BASES.with(|x| x.borrow_mut().push((f, v, hot))); // alloc: Program
    }
}

/// The register pass's results by its input (`carry`): a statement shape lowered again (a new session lowers its private
/// programs anew) takes the functions the pass made for the same pre-pass program, without the pass or its proof. Keyed
/// by the content of the pre-pass program and its counter bases (the pass is a function of both); the oldest of more than
/// `MEMO_KEEP` goes. An entry's program holds, at each function's index, the function the pass made for it (no blocks: it
/// made none), on the program caches (the memo outlives every statement).
static MEMO: pgsync::Mutex<Vec<(u128, std::sync::Arc<gravity_ir::Program>)>> = pgsync::Mutex::new(Vec::new());
const MEMO_KEEP: usize = 32;

const PROGRAM_CACHES: memledger::Owner = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::ProgramCaches));

/// The loop-carried values a function's widest loop may reach with the counters carried through it: the baseline backend
/// pins 9 (tpde frontend `PINNED_ASSIGNMENTS`), and a loop near that limit pays for a carried counter on every row (a
/// stack slot moved per row, or one of the loop's own values pushed out of its register), where a counter left in
/// memory pays one add for each row it counts. A counter past the room stays a memory add.
const CARRY_ROOM: usize = 7;

/// One walk of the pre-pass program and its bases into SipHash (std's `DefaultHasher`, fixed keys: the same content gives
/// the same key in every process) and an FNV-1a lane over its fields beside it: a 128-bit key, as no row hash is involved
/// (KeyHash is the row key's). D3266: two SipHash walks were half of an EXPLAIN ANALYZE statement's extra instructions on
/// small join queries (a session lowers its private programs anew, and so takes this key each time).
fn memo_key(ir: &gravity_ir::Program, bases: &[(Func, Value, Vec<i32>)]) -> u128 {
    use core::hash::{Hash, Hasher};
    // SipHash takes the walk's bytes in runs of `RUN` (its stream is the same however it is cut): a call per field cost more
    // than the hash
    const RUN: usize = 256;
    struct Twin {
        sip: std::hash::DefaultHasher,
        fnv: u64,
        run: [u8; RUN],
        len: usize,
    }
    impl Twin {
        fn put(&mut self, bytes: &[u8], lane: u64) {
            self.fnv = (self.fnv ^ lane).wrapping_mul(0x0000_0100_0000_01b3);
            if self.len + bytes.len() > RUN {
                self.sip.write(&self.run[..self.len]);
                self.len = 0;
            }
            if bytes.len() > RUN {
                self.sip.write(bytes);
            } else {
                self.run[self.len..self.len + bytes.len()].copy_from_slice(bytes);
                self.len += bytes.len();
            }
        }
    }
    impl Hasher for Twin {
        fn write(&mut self, bytes: &[u8]) {
            let lane = bytes.iter().fold(bytes.len() as u64, |h, &b| h.rotate_left(8) ^ u64::from(b));
            self.put(bytes, lane);
        }
        fn write_u8(&mut self, i: u8) {
            self.put(&[i], u64::from(i));
        }
        fn write_u16(&mut self, i: u16) {
            self.put(&i.to_ne_bytes(), u64::from(i));
        }
        fn write_u32(&mut self, i: u32) {
            self.put(&i.to_ne_bytes(), u64::from(i));
        }
        fn write_u64(&mut self, i: u64) {
            self.put(&i.to_ne_bytes(), i);
        }
        fn write_usize(&mut self, i: usize) {
            self.put(&i.to_ne_bytes(), i as u64);
        }
        fn finish(&self) -> u64 {
            let mut sip = self.sip.clone();
            sip.write(&self.run[..self.len]);
            sip.finish()
        }
    }
    let mut h = Twin { sip: std::hash::DefaultHasher::new(), fnv: 0xcbf2_9ce4_8422_2325, run: [0; RUN], len: 0 };
    ir.hash(&mut h);
    bases.hash(&mut h);
    u128::from(h.finish()) << 64 | u128::from(h.fnv)
}

/// The register pass (gravity_ir `promote`) over each planned frame's counters: a counter is an SSA value through the
/// rows, written back where the function returns or calls out, never once per row; a counter no other function touches
/// stays in its register across host calls too (`OWN`). A function the pass's proof (`verify_carried`) rejects keeps
/// its counters in memory. The same pre-pass program takes the memo's functions (`MEMO`). Returns the pre-pass program's
/// key when the pass ran: the program it leaves is a function of that key (`verify` keys it so, without a walk).
pub fn carry(ir: &mut gravity_ir::Program, mcx: Mcx<'_>) -> Option<u128> {
    let bases = BASES.with(|x| core::mem::take(&mut *x.borrow_mut()));
    if bases.is_empty() {
        return None;
    }
    let key = memo_key(ir, &bases);
    let hit = pgsync::lock(&MEMO).iter().find(|m| m.0 == key).map(|m| m.1.clone());
    match hit {
        Some(made) => {
            let m = ir.mint();
            for (i, func) in made.funcs.iter().enumerate().filter(|(_, f)| !f.blocks.is_empty()) {
                ir.funcs[i] = func.clone_in(&m);
            }
        }
        None => promote_counters(ir, &bases, key, mcx),
    }
    for (f, base, hot) in &bases {
        local_bases(&mut ir.funcs[f.index()], *base, hot, mcx);
    }
    Some(key)
}

/// `carry`'s pass on a memo miss: the functions it made filed under `key`.
fn promote_counters(ir: &mut gravity_ir::Program, bases: &[(Func, Value, Vec<i32>)], key: u128, mcx: Mcx<'_>) {
    let mut made: PgVec<'_, Func> = PgVec::new_in(mcx);
    let users = cell_users(ir, mcx);
    let own = |f: Func, off: i32| users.as_ref().is_some_and(|u| u.iter().all(|&(g, o)| o != off || g == f));
    let m = ir.mint();
    let hosts = m.collect(ir.host_fns.iter().map(|h| StateMask(h.flags.state.0 & !OWN.0)));
    // only the counters a row adds to: one added once per window, block or write-back stays a memory add there (carried,
    // it would hold a register, or a stack slot the baseline backend moves per row, through every loop for nothing)
    for &(f, base, ref hot) in bases {
        let mut offs: PgVec<'_, i32> = PgVec::new_in(mcx);
        for i in ir.funcs[f.index()].blocks.iter().flat_map(|bb| bb.insts.iter()) {
            if let Op::Load { ptr, offset, .. } | Op::Store { ptr, offset, .. } = i.op {
                if ptr == base && hot.contains(&offset) && !offs.contains(&offset) && offs.len() < 64 {
                    offs.push_in(offset);
                }
            }
        }
        // the room the function's widest loop leaves: the earliest boundaries first (they count the most rows)
        let fun = &ir.funcs[f.index()];
        let widest = fun.hints.loop_headers.iter().map(|h| fun.blocks[h.index()].params.len()).max().unwrap_or(0);
        let offs: PgVec<'_, i32> = hot.iter().copied().filter(|o| offs.contains(o)).take(CARRY_ROOM.saturating_sub(widest)).collect_in(mcx);
        if offs.is_empty() {
            continue;
        }
        let cells: PgVec<'_, Carried> = offs.iter().map(|&offset| Carried { root: Root::Value(base), offset, ty: Type::I64, class: if own(f, offset) { OWN } else { COUNTERS } }).collect_in(mcx);
        let f0 = ir.dup_func(f);
        // C6 (b): Err(TooDeep) = a content read walked more than PROMOTE_MAX_READ_DEPTH blocks (the function is half
        // rewritten); the function keeps its counters in memory, as on a verify failure (an optimisation, not a refusal)
        let promoted = gravity_ir::promote::promote(&mut ir.funcs[f.index()], &cells, &hosts, gravity_abi::limits::PROMOTE_MAX_READ_DEPTH);
        match promoted {
            Ok(report) if gravity_ir::verify::verify_carried(&m, &f0, &ir.funcs[f.index()], &cells, &report, &hosts).is_ok() => {
                made.push_in(f);
            }
            _ => ir.funcs[f.index()] = f0,
        }
    }
    let mut prog = gravity_ir::Program::new_owned(PROGRAM_CACHES);
    let pm = prog.mint();
    for (i, func) in ir.funcs.iter().enumerate() {
        prog.funcs.push(if made.contains(&Func(i as u32)) { func.clone_in(&pm) } else { pm.function("", &[], &[]) });
    }
    let mut memo = pgsync::lock(&MEMO);
    if !memo.iter().any(|m| m.0 == key) {
        if memo.len() >= MEMO_KEEP {
            memo.remove(0);
        }
        memo.push((key, prog.into())); // alloc: Program
    }
}

/// D3266: the base `open_base` loads at a planned function's entry would stay live to its last use, and the baseline
/// backend holds it in a register through every loop between, where the plain program's loop values had that register
/// (TH18's fold: +18 % instructions under EXPLAIN ANALYZE from spills, with the same loop IR). So each other block that
/// reads or writes a counter loads the base itself, from `ctx` (two loads, invariant for the call); a carried counter's
/// entry load and write-backs included. A block that adds to a counter a row adds to (`hot`) in memory keeps the entry's
/// base, as the loads would run once per row. `false` (unchanged) when the base is used as anything but a counter's
/// address.
fn local_bases(f: &mut Function, base: Value, hot: &[i32], mcx: Mcx<'_>) -> bool {
    let Some(&ctx) = f.blocks.get(f.entry.index()).and_then(|bb| bb.params.first()) else { return false };
    let Some(home) = f.blocks.iter().position(|bb| bb.insts.iter().any(|i| i.result == Some(base))) else { return false };
    let mut ops: Vec<Value> = Vec::new(); // alloc: Program
    let mut users: PgVec<'_, usize> = PgVec::new_in(mcx);
    for (bi, bb) in f.blocks.iter().enumerate() {
        let mut used = false;
        for i in &bb.insts {
            match i.op {
                Op::Load { ptr, .. } | Op::LoadZext { ptr, .. } if ptr == base => used = true,
                Op::Store { ptr, value, .. } if ptr == base && value != base => used = true,
                _ => {
                    ops.clear();
                    i.op.operands_into(&mut ops);
                    if ops.contains(&base) {
                        return false;
                    }
                }
            }
        }
        ops.clear();
        bb.term.operands_into(&mut ops);
        match &bb.term {
            gravity_ir::Terminator::Br(e) => ops.extend_from_slice(&e.args), // alloc: Program
            gravity_ir::Terminator::CondBr { then_edge, else_edge, .. } => [then_edge, else_edge].iter().for_each(|e| ops.extend_from_slice(&e.args)), // alloc: Program
            gravity_ir::Terminator::Switch { cases, default, .. } => cases.iter().map(|c| &c.edge).chain([default]).for_each(|e| ops.extend_from_slice(&e.args)), // alloc: Program
            gravity_ir::Terminator::Return(_) | gravity_ir::Terminator::Unreachable => {}
        }
        if ops.contains(&base) || bb.params.contains(&base) {
            return false;
        }
        let row_add = |off: i32| bb.insts.iter().any(|i| matches!(i.op, Op::Load { ptr, offset, .. } if ptr == base && offset == off));
        let per_row = bb.insts.iter().any(|i| matches!(i.op, Op::Store { ptr, offset, .. } if ptr == base && hot.contains(&offset) && row_add(offset)));
        if used && bi != home && !per_row {
            users.push_in(bi);
        }
    }
    for &bi in &users {
        let (block, local) = (Value(f.values.len() as u32), Value(f.values.len() as u32 + 1));
        f.values.extend([Type::Ptr, Type::Ptr]); // alloc: Program
        let load = |result: Value, ptr: Value, offset: usize| gravity_ir::Instruction { result: Some(result), op: Op::Load { ty: Type::Ptr, ptr, offset: offset as i32, unaligned: false } };
        let insts = &mut f.blocks[bi].insts;
        for i in insts.iter_mut() {
            match &mut i.op {
                Op::Load { ptr, .. } | Op::LoadZext { ptr, .. } | Op::Store { ptr, .. } if *ptr == base => *ptr = local,
                _ => {}
            }
        }
        // the two loads go to the block's front: pushed (growth in the program root) then rotated in place; not a splice,
        // whose drop may collect a leftover tail into a global Vec (alloclint IR leg), nor Extend::extend (a growth row)
        insts.push(load(block, ctx, hdr::PROGRAM)); // alloc: Program
        insts.push(load(local, block, prog::INSTR)); // alloc: Program
        insts.rotate_right(2);
    }
    !users.is_empty()
}

/// Each (function, counter offset) of `ir` that reads or writes a counter through its region's base (`load ptr (load ptr
/// ctx, PROGRAM), INSTR`, `ctx` the function's first parameter); `None` when a base is used as anything but a load's or
/// a store's address (every counter is then taken as shared).
fn cell_users<'m>(ir: &gravity_ir::Program, mcx: Mcx<'m>) -> Option<PgVec<'m, (Func, i32)>> {
    let mut users: PgVec<'m, (Func, i32)> = PgVec::new_in(mcx);
    let mut ops: Vec<Value> = Vec::new(); // alloc: Program
    for (fi, f) in ir.funcs.iter().enumerate() {
        let Some(&ctx) = f.blocks.get(f.entry.index()).and_then(|bb| bb.params.first()) else { continue };
        let loads = |from: &[Value], at: i32| -> PgVec<'m, Value> {
            f.blocks.iter().flat_map(|bb| bb.insts.iter()).filter_map(|i| match i.op {
                Op::Load { ty: Type::Ptr, ptr, offset, .. } if offset == at && from.contains(&ptr) => i.result,
                _ => None,
            }).collect_in(mcx)
        };
        let bases = loads(&loads(&[ctx], hdr::PROGRAM as i32)[..], prog::INSTR as i32);
        if bases.is_empty() {
            continue;
        }
        let fid = Func(fi as u32);
        for bb in &f.blocks {
            if bb.params.iter().any(|v| bases.contains(v)) {
                return None;
            }
            for i in &bb.insts {
                let off = match i.op {
                    Op::Load { ptr, offset, .. } | Op::LoadZext { ptr, offset, .. } if bases.contains(&ptr) => Some(offset),
                    Op::Store { ptr, offset, value, .. } if bases.contains(&ptr) && !bases.contains(&value) => Some(offset),
                    _ => None,
                };
                match off {
                    Some(o) if !users.contains(&(fid, o)) => users.push_in((fid, o)),
                    Some(_) => {}
                    None => {
                        ops.clear();
                        i.op.operands_into(&mut ops);
                        if ops.iter().any(|v| bases.contains(v)) {
                            return None;
                        }
                    }
                }
            }
            ops.clear();
            bb.term.operands_into(&mut ops);
            let edges: PgVec<'_, &gravity_ir::Edge> = match &bb.term {
                gravity_ir::Terminator::Br(e) => mcx::pg_vec![in mcx; e],
                gravity_ir::Terminator::CondBr { then_edge, else_edge, .. } => mcx::pg_vec![in mcx; then_edge, else_edge],
                gravity_ir::Terminator::Switch { cases, default, .. } => cases.iter().map(|c| &c.edge).chain([default]).collect_in(mcx),
                gravity_ir::Terminator::Return(_) | gravity_ir::Terminator::Unreachable => PgVec::new_in(mcx),
            };
            edges.iter().for_each(|e| ops.extend_from_slice(&e.args)); // alloc: Program
            if ops.iter().any(|v| bases.contains(v)) {
                return None;
            }
        }
    }
    Some(users)
}

/// Graph pipeline `k`'s boundaries (`Bounds::of`), or, when they cannot be made, its nodes are counted nowhere. A node
/// whose rows leave before one of the `lead` leading filters is not counted either: the source's verdicts (a zone, the
/// condition cache, a code range) skip a window on those filters whole, and the block path tests them as a mask, so its
/// rows past such a filter are never seen one by one.
pub fn plan_pipeline(k: usize, entries: &[(i32, u16, bool)], nops: usize, hash: bool, lead: usize, own: &dyn Fn(i32) -> bool) {
    if !on() {
        return;
    }
    match Bounds::of(entries, nops, hash, own) {
        Some(b) => {
            uncounted(b.at.iter().take(lead).flatten().copied());
            PLAN.with(|p| {
                let mut p = p.borrow_mut();
                p.active = true;
                p.pipes.push((k, b)); // alloc: Program
            })
        }
        None => uncounted(entries.iter().map(|e| e.0)),
    }
}

/// A spilled join's pair pass `v` (its steps graph pipeline `q`'s ops from `j0`, the probe at `j`): `q` counted its kept
/// rows at every boundary up to the probe, where it kept them, so the pass counts the boundaries past `j` only.
pub fn plan_pass(v: usize, q: usize, j0: usize, j: usize) {
    if !on() {
        return;
    }
    let Some(b) = PLAN.with(|p| p.borrow().pipes.iter().find(|x| x.0 == q).map(|x| x.1.clone())) else { return };
    if j0 > j || j >= b.at.len() {
        return;
    }
    let mut at: Vec<Vec<i32>> = b.at[j0..].to_vec(); // alloc: Program
    at.iter_mut().take(j - j0 + 1).for_each(Vec::clear);
    PLAN.with(|p| p.borrow_mut().pipes.push((v, Bounds { at, end: b.end, loops: Vec::new(), out: false, win: false, src: None }))); // alloc: Program
}

/// A nest's embedded inner pipeline `ik`, run inside pipeline `k`'s function per outer row (rerun/inner.rs): its
/// boundaries as any pipeline's (`plan_pipeline`), its loops counted at each re-open (`count_loops`) instead of a
/// function's open, its time its owner's.
pub fn plan_inner(ik: usize, k: usize, entries: &[(i32, u16, bool)], nops: usize, lead: usize) {
    if !on() {
        return;
    }
    plan_pipeline(ik, entries, nops, false, lead, &|_| false);
    PLAN.with(|p| p.borrow_mut().inners.push((ik, k))); // alloc: Program
}

/// The tag bit that keeps an arm key apart from every pipeline index: bit 40 on 64-bit targets, the second-highest bit
/// of a 32-bit usize (wasm32).
const ARM_KEY_TAG: usize = 1 << (if usize::BITS > 40 { 40 } else { usize::BITS - 2 });

/// The plan's key of union arm `kf` streamed inside a nest inner (rerun/inner_arms.rs): its feed pipeline `kf` runs
/// nothing itself, so its boundaries are kept apart from that pipeline's (which counts none).
pub fn arm_key(kf: usize) -> usize {
    ARM_KEY_TAG | kf
}

/// Pipeline `ik`'s boundaries once the plan is sealed (an embedded inner's, which `swap` makes current while its rows
/// lower), `None` when it has none or the lowering is not instrumented.
pub fn bounds_of(ik: usize) -> Option<Bounds> {
    if !on() {
        return None;
    }
    PLAN.with(|p| p.borrow().pipes.iter().find(|q| q.0 == ik).map(|q| q.1.clone()))
}

/// The current boundaries replaced by `b` (rerun/inner.rs `swap`: an inner's rows lower under its own, its owner's
/// continuation under the owner's); the ones replaced.
pub fn replace_current(b: Option<Bounds>) -> Option<Bounds> {
    CURRENT.with(|c| core::mem::replace(&mut *c.borrow_mut(), b))
}

/// One loop of each node of the current boundaries' starts that `which` keeps (an embedded inner's re-open: the nodes at
/// and above a Memoize or a one-time gate at every re-open, those below it only where the source opens).
pub fn count_loops(b: &mut FunctionBuilder<'_>, ctx: Value, which: &dyn Fn(i32) -> bool, mcx: Mcx<'_>) {
    if !on() {
        return;
    }
    let nodes: PgVec<'_, i32> = CURRENT.with(|c| c.borrow().as_ref().map(|x| x.loops.iter().copied().filter(|&n| which(n)).collect_in(mcx)).unwrap_or_else(|| PgVec::new_in(mcx)));
    if nodes.is_empty() {
        return;
    }
    let one = b.iconst(Type::I64, 1);
    nodes.into_iter().for_each(|x| add(b, ctx, x, Ctr::Loops, one, false));
}

/// Whether node `node`'s quals count their failures (`Ctr::Filtered1`) in the current frame: a bounded frame's source node
/// its boundaries count (a scan's Filter: rows it reads and drops), or, in a frame without boundaries, the frame's node
/// (`frame`; step/filter.rs `quals_then`).
pub fn filters_counted(node: i32, frame: i32) -> bool {
    on() && match CURRENT.with(|c| c.borrow().as_ref().map(|b| b.src)) {
        Some(src) => src == Some(node) && counted(node),
        None => node == frame,
    }
}

/// `instr[cell(node, Filtered1)] += popcnt(before) - popcnt(after)`: the rows a block's mask step dropped, where the
/// node's failures count (`filters_counted`).
pub fn count_mask_dropped(b: &mut FunctionBuilder<'_>, ctx: Value, node: i32, frame: i32, before: Value, after: Value) {
    if !filters_counted(node, frame) {
        return;
    }
    let (a, k) = (b.int_un(IntUnOp::Popcnt, before), b.int_un(IntUnOp::Popcnt, after));
    let n = b.int_bin(IntBinOp::Sub, a, k);
    add(b, ctx, node, Ctr::Filtered1, n, true);
}

/// Pipeline `v` has boundaries (`plan_pipeline`, `plan_pass`).
pub fn planned_pipe(v: usize) -> bool {
    PLAN.with(|p| p.borrow().pipes.iter().any(|x| x.0 == v))
}

/// Nodes some function runs out of their pipeline's order (a swapped side, a fused tail, a spilled pass): no boundary
/// counts them.
pub fn uncounted(nodes: impl Iterator<Item = i32>) {
    if !on() {
        return;
    }
    PLAN.with(|p| {
        let mut p = p.borrow_mut();
        p.active = true;
        for n in nodes {
            if !p.unsafe_nodes.contains(&n) {
                p.unsafe_nodes.push(n); // alloc: Program
            }
        }
    });
}

/// The runtime filter of the join probing at op `j` of graph pipeline `k` (join.rtfilter) skips the scan's granules no
/// build key can match, unread: the rows a row of such a granule would have counted before the probe (the scan's, and
/// any node's between) are unknown, so those nodes are not counted.
pub fn cut(k: usize, j: usize) {
    if on() {
        PLAN.with(|p| p.borrow_mut().cuts.push((k, j))); // alloc: Program
    }
}

/// Seals the plan once every pipeline is resolved, `order` the program's pipelines in emit order: an uncountable node
/// leaves every boundary, and a node's `Loops` are counted by the first pipeline (in `order`) that starts it, or by every
/// embedded inner that re-opens it where no program pipeline does.
pub fn seal_plan(order: &[usize], mcx: Mcx<'_>) {
    PLAN.with(|p| {
        let mut p = p.borrow_mut();
        let Plan { pipes, unsafe_nodes, counted, cuts, alias, members, .. } = &mut *p;
        for &(k, j) in cuts.iter() {
            let Some((_, b)) = pipes.iter().find(|q| q.0 == k) else { continue };
            for n in b.at.iter().take(j + 1).flatten() {
                if !unsafe_nodes.contains(n) {
                    unsafe_nodes.push(*n); // alloc: Program
                }
            }
        }
        let bad = |n: &i32| unsafe_nodes.contains(n);
        let mut owned: PgVec<'_, i32> = PgVec::new_in(mcx);
        // the program's pipelines in order, then the embedded inners (`plan_inner`), which no function opens
        let rest: PgVec<'_, usize> = pipes.iter().map(|q| q.0).filter(|k| !order.contains(k)).collect_in(mcx);
        for &k in order.iter().chain(rest.iter()) {
            let Some((_, b)) = pipes.iter_mut().find(|q| q.0 == k) else { continue };
            let program = order.contains(&k);
            let skip = |n: &i32| bad(n) || (program && reader_only(*n));
            b.at.iter_mut().for_each(|a| a.retain(|n| !skip(n)));
            b.end.retain(|n| !skip(n));
            b.loops.retain(|n| !skip(n) && !owned.contains(n));
            // an embedded inner counts its loops at its own re-opens, so copies of one inner (a write's arms, each with a
            // nest copy over the one plan Material: decompose readers.rs `copies`) each count theirs, as PostgreSQL's
            // Materialize rescans once per outer row of every arm: only a program pipeline owns a node's loops
            if order.contains(&k) {
                owned.extend_in(b.loops.iter().copied());
            }
            for n in b.at.iter().flatten().chain(b.end.iter()) {
                if !counted.contains(n) {
                    counted.push(*n); // alloc: Program
                }
            }
        }
        *members = pipes.iter().flat_map(|(k, b)| b.at.iter().flatten().chain(b.end.iter()).chain(b.loops.iter()).map(move |&n| (*k, n))).collect(); // alloc: Program
        // one counter per boundary: every node a boundary counts takes the same rows, so a later node of the boundary
        // takes its first node's (`Ctr::Same`) where it is in no other boundary and the first is first wherever it is
        let lists: PgVec<'_, &Vec<i32>> = pipes.iter().flat_map(|(_, b)| b.at.iter().chain(core::iter::once(&b.end))).collect_in(mcx);
        let times = |n: i32| lists.iter().flat_map(|l| l.iter()).filter(|&&x| x == n).count();
        for l in lists.iter() {
            let Some((&a, rest)) = l.split_first() else { continue };
            if lists.iter().any(|m| m.iter().skip(1).any(|&x| x == a)) {
                continue;
            }
            alias.extend(rest.iter().filter(|&&n| times(n) == 1).map(|&n| (n, a))); // alloc: Program
        }
        drop(lists);
        let later = |n: &i32| alias.iter().any(|x| x.0 == *n);
        for (_, b) in pipes.iter_mut() {
            b.at.iter_mut().for_each(|l| l.retain(|n| !later(n)));
            b.end.retain(|n| !later(n));
        }
    });
}

/// The program's pipelines in run order (`order[p]` = the graph pipeline program pipeline `p` runs): each counted node's
/// program pipelines, for the `Pipe` / `PipeOf` cells EXPLAIN ANALYZE reads a node's time from.
pub fn pipe_order(order: &[usize]) {
    if !on() {
        return;
    }
    PLAN.with(|p| {
        let mut p = p.borrow_mut();
        // an embedded inner's pipeline is its owner's (an inner of an inner: its owner's owner's, and so on)
        let run_by = |mut k: usize| {
            for _ in 0..p.inners.len() {
                match p.inners.iter().find(|x| x.0 == k) {
                    Some(&(_, o)) => k = o,
                    None => break,
                }
            }
            order.iter().position(|&x| x == k)
        };
        let mut of: Vec<(i32, u16)> = Vec::new(); // alloc: Program
        for &(k, n) in p.members.iter() {
            if let Some(i) = run_by(k) {
                if !of.contains(&(n, i as u16)) {
                    of.push((n, i as u16)); // alloc: Program
                }
            }
        }
        p.pipes_of = of;
    });
}

/// The frame of graph pipeline `k` is lowered between `enter(k)` and the guard's drop (nested frames keep their own).
pub fn enter(k: usize) -> impl Drop {
    struct Restore(Option<Bounds>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let prev = self.0.take();
            CURRENT.with(|c| *c.borrow_mut() = prev);
        }
    }
    let b = if on() { PLAN.with(|p| p.borrow().pipes.iter().find(|q| q.0 == k).map(|q| q.1.clone())) } else { None };
    Restore(CURRENT.with(|c| core::mem::replace(&mut *c.borrow_mut(), b)))
}

/// The current frame counts its end's rows from the write-backs' `out` (`EndMech::out_counts_rows`): set by the frame
/// where every row the end takes passes a write-back (no step that holds rows back, a windowed source).
pub fn by_out(on: bool) {
    CURRENT.with(|c| {
        if let Some(b) = c.borrow_mut().as_mut() {
            b.out = on && !b.end.is_empty();
        }
    });
}

/// The current frame counts `at[0]` (the nodes a row leaves as the source hands it on) once per window, from the window's
/// positions (`count_window`): set by the frame at its entry where each position of a window is a row that enters step 0
/// unless a stop leaves it unread (a windowed source, no step that holds rows back). Its word, the end of the window
/// counted ahead, is 0 outside a window.
pub fn by_window(b: &mut FunctionBuilder<'_>, on: bool) {
    let set = CURRENT.with(|c| match c.borrow_mut().as_mut() {
        Some(x) => {
            x.win = on && x.at.first().is_some_and(|n| !n.is_empty());
            x.win
        }
        None => false,
    });
    if set {
        let slot = b.declare_stack_slot(8, 8);
        let at = b.stack_addr(slot);
        let zero = b.iconst(Type::I64, 0);
        b.store(at, 0, zero);
        AHEAD.with(|x| x.set(Some((b.func() as *const Function as usize, at))));
    }
}

fn ahead(b: &FunctionBuilder<'_>) -> Option<Value> {
    let here = b.func() as *const Function as usize;
    AHEAD.with(Cell::get).filter(|a| a.0 == here && CURRENT.with(|c| c.borrow().as_ref().is_some_and(|x| x.win))).map(|a| a.1)
}

/// A window `[cursor, end)` the source handed on: each position a row entering step 0, counted ahead for `at[0]`.
pub fn count_window(b: &mut FunctionBuilder<'_>, ctx: Value, cursor: Value, end: Value, mcx: Mcx<'_>) {
    let Some(at) = ahead(b) else { return };
    let n = b.int_bin(IntBinOp::Sub, end, cursor);
    at_step(0, mcx).into_iter().for_each(|x| count_bound(b, ctx, x, n));
    b.store(at, 0, end);
}

/// A write-back at cursor `cur`: the window's positions from `cur` on were counted ahead but not read here (a resumed call
/// counts them again from its cursor), so `at[0]` takes them back; the word goes back to 0.
pub fn uncount_ahead(b: &mut FunctionBuilder<'_>, ctx: Value, cur: Value, mcx: Mcx<'_>) {
    let Some(at) = ahead(b) else { return };
    let end = b.load(Type::I64, at, 0);
    let rest = b.int_bin(IntBinOp::Sub, end, cur);
    let zero = b.iconst(Type::I64, 0);
    let rest = b.int_bin(IntBinOp::Smax, rest, zero);
    let back = b.int_bin(IntBinOp::Sub, zero, rest);
    at_step(0, mcx).into_iter().for_each(|x| count_bound(b, ctx, x, back));
    b.store(at, 0, zero);
}

/// The nodes a row entering step `j` counts where it enters (`at_step`), but `at[0]` where windows count it.
pub fn row_at<'m>(j: usize, mcx: Mcx<'m>) -> PgVec<'m, i32> {
    match j == 0 && CURRENT.with(|c| c.borrow().as_ref().is_some_and(|x| x.win)) {
        true => PgVec::new_in(mcx),
        false => at_step(j, mcx),
    }
}

/// Step `j` is the current frame's end and its rows are counted from `out` (`by_out`).
pub fn end_by_out(j: usize) -> bool {
    CURRENT.with(|c| c.borrow().as_ref().is_some_and(|b| b.out && j == b.at.len()))
}

/// The end's rows a write-back hands on (`out` since the window's or the resume's start), where they are its count.
pub fn count_end_out(b: &mut FunctionBuilder<'_>, ctx: Value, out: Value, mcx: Mcx<'_>) {
    let nodes = CURRENT.with(|c| c.borrow().as_ref().filter(|b| b.out).map(|b| mcx::to_vec_in(&b.end[..], mcx)));
    nodes.into_iter().flatten().for_each(|x| count_bound(b, ctx, x, out));
}

/// A row the end took but keeps no record of (a hash build's NULL key under a join that keeps no NULL inner row:
/// PostgreSQL's Hash neither inserts nor counts it, `MultiExecPrivateHash`): the end's count takes it back. Its rows
/// counted per row only (an end counted from `out` adds nothing for such a row).
pub fn uncount_end_row(b: &mut FunctionBuilder<'_>, ctx: Value) {
    let nodes = CURRENT.with(|c| c.borrow().as_ref().filter(|x| !x.out).map(|x| x.end.clone())).unwrap_or_default();
    if nodes.is_empty() {
        return;
    }
    let back = b.iconst(Type::I64, -1);
    nodes.into_iter().for_each(|x| count_row(b, ctx, x, back));
}

/// Runs `f` with no boundary counted (a recheck's stored row enters past the ops it never ran).
pub fn unbounded<T>(f: impl FnOnce() -> T) -> T {
    let prev = CURRENT.with(|c| c.borrow_mut().take());
    let v = f();
    CURRENT.with(|c| *c.borrow_mut() = prev);
    v
}

/// The nodes a row entering step `j` of the current frame counts (`Bounds::at`, and `end` at `nops`).
pub fn at_step<'m>(j: usize, mcx: Mcx<'m>) -> PgVec<'m, i32> {
    CURRENT.with(|c| match &*c.borrow() {
        Some(b) if j < b.at.len() => mcx::to_vec_in(&b.at[j][..], mcx),
        Some(b) if j == b.at.len() => mcx::to_vec_in(&b.end[..], mcx),
        _ => PgVec::new_in(mcx),
    })
}

/// The current frame's run ends before its input does where PostgreSQL's consumer reads all of it: none of its nodes is
/// counted.
pub fn partial() {
    if on() {
        all_nodes().into_iter().for_each(|n| drop(cell(n, Ctr::Partial)));
    }
}

/// No node of the current frame leaves before step `span` (a block path's masked steps) but `at[0]` where windows count
/// it (`by_window`): true outside a bounded frame.
pub fn clear_before(span: usize) -> bool {
    CURRENT.with(|c| c.borrow().as_ref().is_none_or(|b| b.at.iter().take(span).enumerate().all(|(i, n)| n.is_empty() || (i == 0 && b.win))))
}

/// `n` rows past every boundary of the current frame (a block the end folds whole: every row it holds reaches the end),
/// but the end's where its write-backs count them (`by_out`).
pub fn count_all(b: &mut FunctionBuilder<'_>, ctx: Value, n: Value, mcx: Mcx<'_>) {
    if bounded() {
        row_nodes(mcx).into_iter().for_each(|x| count_bound(b, ctx, x, n));
    }
}

/// `count_all` of a block's live rows, its mask's popcount (made only in an instrumented program: the plain one is
/// unchanged).
pub fn count_mask(b: &mut FunctionBuilder<'_>, ctx: Value, mask: Value, mcx: Mcx<'_>) {
    if bounded() && !row_nodes(mcx).is_empty() {
        let n = b.int_un(IntUnOp::Popcnt, mask);
        count_all(b, ctx, n, mcx);
    }
}

/// `all_nodes` but the end's where its write-backs count them (`by_out`) and `at[0]`'s where windows do (`by_window`).
pub fn row_nodes<'m>(mcx: Mcx<'m>) -> PgVec<'m, i32> {
    CURRENT.with(|c| {
        c.borrow().as_ref().map_or_else(|| PgVec::new_in(mcx), |b| {
            let at = b.at.iter().enumerate().filter(|(i, _)| *i > 0 || !b.win).flat_map(|(_, n)| n.iter());
            at.chain(b.end.iter().filter(|_| !b.out)).copied().collect_in(mcx)
        })
    })
}

/// The rows a block test dropped as they entered step `j` (a probe's build filter on the block path: `all` the block's
/// mask, `kept` the rows it keeps): they left the nodes a row entering `j` counts (`at_step`), as the kept rows do
/// when they walk on.
pub fn count_dropped(b: &mut FunctionBuilder<'_>, ctx: Value, j: usize, all: Value, kept: Value, mcx: Mcx<'_>) {
    let nodes = row_at(j, mcx);
    if nodes.is_empty() {
        return;
    }
    let (a, k) = (b.int_un(IntUnOp::Popcnt, all), b.int_un(IntUnOp::Popcnt, kept));
    let n = b.int_bin(IntBinOp::Sub, a, k);
    nodes.into_iter().for_each(|x| count_bound(b, ctx, x, n));
}

/// Every node the current frame counts rows of.
pub fn all_nodes() -> Vec<i32> {
    CURRENT.with(|c| c.borrow().as_ref().map_or_else(Vec::new, |b| b.at.iter().flatten().chain(b.end.iter()).copied().collect()))
}

/// The current frame is bounded: its nodes' rows are counted at its boundaries.
pub fn bounded() -> bool {
    CURRENT.with(|c| c.borrow().is_some())
}

fn planned() -> bool {
    PLAN.with(|p| p.borrow().active)
}

fn counted(node: i32) -> bool {
    PLAN.with(|p| p.borrow().counted.contains(&node))
}

/// `instr[cell(node, Rows)] += n` for a node a boundary counts, once per window, block or write-back (a memory add).
pub fn count_bound(b: &mut FunctionBuilder<'_>, ctx: Value, node: i32, n: Value) {
    if on() {
        add(b, ctx, node, Ctr::Rows, n, false);
    }
}

/// `count_bound` for a row: the counter is carried in a register through the function's loops (`carry`).
pub fn count_row(b: &mut FunctionBuilder<'_>, ctx: Value, node: i32, n: Value) {
    if on() {
        add(b, ctx, node, Ctr::Rows, n, true);
    }
}

/// The rows a frame without a plan (`resolve` makes one for every instrumented program) hands its end: `out` rows of
/// `node` at a write-back.
pub fn count_out(b: &mut FunctionBuilder<'_>, ctx: Value, node: i32, out: Value) {
    if !planned() {
        count(b, ctx, node, Ctr::Rows, out);
    }
}

/// `f` lowered instrumented or not; the cells it allocated, in index order (empty when not instrumented). The flag is
/// cleared on every exit, a refusal's unwind included.
pub fn scope<T>(instrument: bool, f: impl FnOnce() -> T) -> (T, Vec<InstrCell>) {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            ON.with(|o| o.set(false));
            PLAN.with(|p| *p.borrow_mut() = Plan::default());
            CURRENT.with(|c| *c.borrow_mut() = None);
            BASE.with(|x| x.set(None));
            AHEAD.with(|x| x.set(None));
            HOT.with(|h| h.borrow_mut().clear());
            BASES.with(|x| x.borrow_mut().clear());
        }
    }
    CELLS.with(|c| c.borrow_mut().clear());
    PLAN.with(|p| *p.borrow_mut() = Plan::default());
    ON.with(|o| o.set(instrument));
    let reset = Reset;
    let v = f();
    // each later node of a boundary names the node whose rows it takes, in the two cells after the lowering's own; then
    // each counted node's program pipelines (`pipe_order`), two cells each
    let alias = PLAN.with(|p| core::mem::take(&mut p.borrow_mut().alias));
    let pipes_of = PLAN.with(|p| core::mem::take(&mut p.borrow_mut().pipes_of));
    CELLS.with(|c| {
        let mut c = c.borrow_mut();
        for (n, a) in alias {
            c.push(InstrCell { node: n, ctr: Ctr::Same }); // alloc: Program
            c.push(InstrCell { node: a, ctr: Ctr::SameAs }); // alloc: Program
        }
        for (n, p) in pipes_of {
            c.push(InstrCell { node: n, ctr: Ctr::Pipe }); // alloc: Program
            c.push(InstrCell { node: p as i32, ctr: Ctr::PipeOf }); // alloc: Program
        }
    });
    drop(reset);
    (v, CELLS.with(|c| core::mem::take(&mut *c.borrow_mut())))
}

/// The lowering is instrumented (a mechanism computing a count it would not otherwise have asks first).
pub fn on() -> bool {
    ON.with(Cell::get)
}

/// The cell of `(node, ctr)`, allocated on first use.
pub fn cell(node: i32, ctr: Ctr) -> u32 {
    CELLS.with(|c| {
        let mut c = c.borrow_mut();
        let i = c.iter().position(|x| x.node == node && x.ctr == ctr).unwrap_or_else(|| {
            c.push(InstrCell { node, ctr }); // alloc: Program
            c.len() - 1
        });
        i as u32
    })
}

/// `instr[cell(node, ctr)] += n` (`n` an `I64`) in the current block; nothing when the lowering is not instrumented, nor
/// for the rows of a node its boundaries count.
pub fn count(b: &mut FunctionBuilder<'_>, ctx: Value, node: i32, ctr: Ctr, n: Value) {
    if !on() || (ctr == Ctr::Rows && counted(node)) {
        return;
    }
    add(b, ctx, node, ctr, n, false);
}

/// The add; `hot`: a row's (the function's base then carries the counter: `HOT`).
fn add(b: &mut FunctionBuilder<'_>, ctx: Value, node: i32, ctr: Ctr, n: Value, hot: bool) {
    let off = cell(node, ctr) as i32 * 8;
    let base = cell_base(b, ctx, off, hot);
    let v = b.load(Type::I64, base, off);
    let v = b.int_bin(IntBinOp::Add, v, n);
    b.store(base, off, v);
}

/// A write's node starts once: PostgreSQL's ModifyTable runs once per statement (a CTE's write to its end), while the
/// write's open here runs once per call (a step's prologue runs on a resumed call too), so its `Loops` is set, not added.
pub fn count_started(b: &mut FunctionBuilder<'_>, ctx: Value, node: i32) {
    if !on() {
        return;
    }
    let off = cell(node, Ctr::Loops) as i32 * 8;
    let base = cell_base(b, ctx, off, false);
    let one = b.iconst(Type::I64, 1);
    b.store(base, off, one);
}

/// The counter block a cell at `off` is read and written through: the function's carried base, else the program's.
fn cell_base(b: &mut FunctionBuilder<'_>, ctx: Value, off: i32, hot: bool) -> Value {
    let here = b.func() as *const Function as usize;
    match BASE.with(Cell::get) {
        Some((f, v)) if f == here => {
            if hot {
                HOT.with(|h| {
                    let mut h = h.borrow_mut();
                    if !h.contains(&off) {
                        h.push(off); // alloc: Program
                    }
                });
            }
            v
        }
        _ => {
            let block = b.load(Type::Ptr, ctx, hdr::PROGRAM as i32);
            b.load(Type::Ptr, block, prog::INSTR as i32)
        }
    }
}

/// A pipeline's open counts one `Loops` of `node` (of each node a bounded frame starts), unless the call is a later
/// claim of the pipeline's run (`FLAG_LATER_CLAIM`, D2881): a run by claims opens once per claim, where PostgreSQL's node
/// starts once (a parallel section's claims are settled at its close: the driver's `settle_loops`). `closers`: the steps
/// whose `closes` holds when the call pulls nothing (a false one-time gate, a zero or met LIMIT): PostgreSQL never calls
/// a node below such a step, so the nodes whose rows leave before it (`Bounds::at` up to it, and `Bounds::end` for a step
/// past every op) start only while it is open; the step's own node and those above it start either way.
pub fn count_open(b: &mut FunctionBuilder<'_>, ctx: Value, node: i32, closers: &[(usize, Value)], mcx: Mcx<'_>) {
    if !on() {
        return;
    }
    let cur = CURRENT.with(|c| c.borrow().as_ref().map(|b| (mcx::to_vec_in(&b.loops[..], mcx), b.at.iter().map(|a| mcx::to_vec_in(&a[..], mcx)).collect_in(mcx), mcx::to_vec_in(&b.end[..], mcx))));
    let nodes = cur.as_ref().map(|c| &c.0);
    if nodes.as_ref().is_some_and(|n| n.is_empty()) || (nodes.is_none() && planned() && counted(node)) {
        return;
    }
    let fl = b.load_zext(Type::I32, ctx, hdr::FLAGS as i32);
    let bit = b.iconst(Type::I64, gravity_abi::ctx::FLAG_LATER_CLAIM as i64);
    let later = b.int_bin(IntBinOp::And, fl, bit);
    let (zero, one) = (b.iconst(Type::I64, 0), b.iconst(Type::I64, 1));
    let first = b.icmp(IntCond::Eq, later, zero);
    let n = b.select(first, one, zero);
    match cur {
        Some((ns, at, end)) => ns.into_iter().for_each(|x| {
            // D3422: a step past every op (the feeder of the checks above the pipeline, a fused tail's) shuts the end's
            // nodes too: its node is above all of them
            let shuts = |j: usize| at.iter().take(j + 1).any(|a| a.contains(&x)) || (j >= at.len() && end.contains(&x));
            let below = closers.iter().filter(|(j, _)| shuts(*j)).map(|c| c.1);
            let shut = below.reduce(|a, c| b.int_bin(IntBinOp::Or, a, c));
            let nx = shut.map_or(n, |s| b.select(s, zero, n));
            add(b, ctx, x, Ctr::Loops, nx, false)
        }),
        None => count(b, ctx, node, Ctr::Loops, n),
    }
}

pub fn count_one(b: &mut FunctionBuilder<'_>, ctx: Value, node: i32, ctr: Ctr) {
    if !on() {
        return;
    }
    if ctr == Ctr::Rows && counted(node) {
        return;
    }
    let one = b.iconst(Type::I64, 1);
    add(b, ctx, node, ctr, one, ctr != Ctr::Loops);
}

#[cfg(test)]
mod tests {
    use super::Bounds;

    #[test]
    fn a_node_counts_where_the_next_node_starts() {
        // a scan (6) with its qual (op 0) under a Hash (5) whose build is op 1: the scan's rows enter op 1, the Hash's reach
        // the end (a hash build counts its producer-top entry)
        let b = Bounds::of(&[(6, 0, false), (5, 1, false)], 2, true, &|_| false).unwrap();
        assert_eq!(b, Bounds { at: vec![vec![], vec![6]], end: vec![5], loops: vec![6, 5], out: false, win: false, src: Some(6) });
        // an aggregate's emit (2) into a Sort (1, producer-top, not a hash): the Sort's rows are its own emit's
        let b = Bounds::of(&[(2, 0, false), (1, 1, true)], 2, false, &|_| false).unwrap();
        assert_eq!(b, Bounds { at: vec![vec![], vec![2]], end: vec![], loops: vec![2], out: false, win: false, src: Some(2) });
        // a node of no op passes its rows on where the next one starts: both count at op 1
        let b = Bounds::of(&[(4, 0, false), (3, 1, false), (2, 1, false)], 2, false, &|_| false).unwrap();
        assert_eq!(b, Bounds { at: vec![vec![], vec![4, 3]], end: vec![2], loops: vec![4, 3, 2], out: false, win: false, src: Some(4) });
        // a bitmap heap scan (6) after its index scan (7), both at op 0: the heap scan owns the source op, its quals' failures
        // are its own
        let b = Bounds::of(&[(7, 0, false), (6, 0, false), (5, 1, false)], 2, false, &|_| false).unwrap();
        assert_eq!(b.src, Some(6));
    }

    /// `f(ctx)`: the base loaded at the entry, a loop whose body adds to a counter, an exit that writes one; `edge` passes
    /// the base to the exit as an argument too.
    fn counted_loop(edge: bool) -> (gravity_ir::Program, gravity_ir::Value) {
        use gravity_ir::{builder::ProgramBuilder, IntBinOp, IntCond, Type};
        let mut pb = ProgramBuilder::new();
        let f = pb.declare_func("f", &[Type::Ptr], &[Type::I64]);
        let base;
        {
            let mut b = pb.define(f);
            let ctx = b.params()[0];
            let block = b.load(Type::Ptr, ctx, super::hdr::PROGRAM as i32);
            base = b.load(Type::Ptr, block, super::prog::INSTR as i32);
            let (head, body) = (b.create_block(&[Type::I64]), b.create_block(&[]));
            let exit = if edge { b.create_block(&[Type::Ptr]) } else { b.create_block(&[]) };
            let (zero, one, ten) = (b.iconst(Type::I64, 0), b.iconst(Type::I64, 1), b.iconst(Type::I64, 10));
            b.br(head, &[zero]);
            b.switch_to(head);
            let i = b.block_params(head)[0];
            let more = b.icmp(IntCond::Slt, i, ten);
            let (none, with): (&[gravity_ir::Value], [gravity_ir::Value; 1]) = (&[], [base]);
            let out: &[gravity_ir::Value] = if edge { &with } else { none };
            b.cond_br(more, (body, none), (exit, out));
            b.switch_to(body);
            let v = b.load(Type::I64, base, 8);
            let v = b.int_bin(IntBinOp::Add, v, one);
            b.store(base, 8, v);
            let next = b.int_bin(IntBinOp::Add, i, one);
            b.br(head, &[next]);
            b.switch_to(exit);
            let at = if edge { b.block_params(exit)[0] } else { base };
            b.store(at, 16, one);
            b.ret(&[zero]);
        }
        (pb.finish(), base)
    }

    #[test]
    fn each_block_loads_the_counter_base_it_uses() {
        use gravity_ir::{Func, Op, Type};
        let rig = mcx::MemoryContext::new("test");
        let (mut ir, base) = counted_loop(false);
        assert!(super::local_bases(&mut ir.funcs[0], base, &[], rig.mcx()));
        gravity_ir::verify::verify(&ir).unwrap();
        let f = &ir.funcs[0];
        let ctx = f.blocks[f.entry.index()].params[0];
        let mut users = 0;
        for (bi, bb) in f.blocks.iter().enumerate().filter(|(bi, _)| *bi != f.entry.index()) {
            let mut ops = Vec::new();
            bb.insts.iter().for_each(|i| i.op.operands_into(&mut ops));
            assert!(!ops.contains(&base), "block {bi} still reads the entry's base");
            if let [a, c, ..] = &bb.insts[..] {
                if let (Op::Load { ty: Type::Ptr, ptr, offset: 0, .. }, Op::Load { ty: Type::Ptr, ptr: p2, offset, .. }) = (&a.op, &c.op) {
                    assert!(*ptr == ctx && Some(*p2) == a.result && *offset == super::prog::INSTR as i32);
                    users += 1;
                }
            }
        }
        assert_eq!(users, 2);
        let (mut ir, base) = counted_loop(true);
        let before = ir.dup_func(Func(0));
        assert!(!super::local_bases(&mut ir.funcs[0], base, &[], rig.mcx()));
        assert_eq!(ir.funcs[0], before);
        // the loop's counter is a row's, left in memory: its block keeps the entry's base, the exit's write loads its own
        let (mut ir, base) = counted_loop(false);
        assert!(super::local_bases(&mut ir.funcs[0], base, &[8], rig.mcx()));
        let f = &ir.funcs[0];
        let reads = |bi: usize| f.blocks[bi].insts.iter().any(|i| matches!(i.op, Op::Load { ptr, .. } | Op::Store { ptr, .. } if ptr == base));
        assert_eq!((1..f.blocks.len()).filter(|&bi| reads(bi)).count(), 1);
    }

    #[test]
    fn the_memo_key_is_the_content() {
        let ((a, va), (b, vb)) = (counted_loop(false), counted_loop(true));
        let bases = |v| vec![(gravity_ir::Func(0), v, vec![8])];
        let k = super::memo_key(&a, &bases(va));
        assert_eq!(k, super::memo_key(&counted_loop(false).0, &bases(va)));
        assert_ne!(k, super::memo_key(&b, &bases(vb)));
        assert_ne!(k, super::memo_key(&a, &[(gravity_ir::Func(0), va, vec![16])]));
        assert_ne!(k >> 64, k & u128::from(u64::MAX));
    }

    #[test]
    fn no_bounds_out_of_order_and_a_write_counts_its_own() {
        assert_eq!(Bounds::of(&[(4, 1, false), (3, 0, false)], 2, false, &|_| false), None);
        assert_eq!(Bounds::of(&[(4, 0, false), (3, 3, false)], 2, false, &|_| false), None);
        let b = Bounds::of(&[(2, 0, false), (1, 1, false)], 2, false, &|n| n == 1).unwrap();
        assert_eq!(b, Bounds { at: vec![vec![], vec![2]], end: vec![], loops: vec![2], out: false, win: false, src: Some(2) });
    }
}

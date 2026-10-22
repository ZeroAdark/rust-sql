//! Register state: promote marked memory cells to SSA values carried in
//! block parameters, written back only on exit edges
//! (`docs/design/gravity-simple/register-state.md` §1).
//!
//! The lowerer names the cells (`Cell`: a root pointer, an offset, the
//! memory type, a `StateMask` class). The pass replaces every load of a
//! cell by the cell's current SSA content, deletes every store (the stored
//! value becomes the content), and inserts a store of the content right
//! before every `Return`, every host call whose `HostFlags::state` names
//! the cell's class and every intra-program `Call`; after such a host the
//! cell is reloaded. SSA construction is Braun et al.'s (blocks in reverse
//! post-order, incomplete phis at unsealed loop headers, trivial phis
//! removed by a fixpoint); a phi is a block parameter appended to the
//! block and an argument appended to every incoming edge. No block is
//! added or reordered. `verify::verify_carried` proves the result against
//! the pre-pass function without this module's bookkeeping.

use crate::*;

/// Where a cell's pointer comes from.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Root {
    /// The pointer is this value (the ctx parameter, a record pointer
    /// defined once per call). Generated code must reach the cell's
    /// memory through this value only.
    Value(Value),
    /// The pointer is loaded from `ptr + offset` (`Type::Ptr`), any number
    /// of times; nothing in the function stores to that slot.
    LoadedFrom { ptr: Value, offset: i32 },
}

/// A cell to carry in registers.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct Cell {
    pub root: Root,
    pub offset: i32,
    /// The memory type: every plain load and every store of the cell has
    /// it; a `LoadZext` of a narrow cell zero-extends it to `I64`.
    pub ty: Type,
    pub class: StateMask,
}

/// C6 (b): [`promote`] stops instead of running the thread off its stack. Reading a cell's content
/// walks the predecessors (Braun et al.'s readVariable, recursive through `add_phi_operands`): one
/// level per block between a use and the content's definition, so a long chain of blocks is a deep
/// walk. The caller's `max_read_depth` levels deep (the lowerer passes gravity_abi
/// `limits::PROMOTE_MAX_READ_DEPTH`) the walk gives up; the function is then half rewritten and the
/// caller keeps its own copy (the unpromoted function).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TooDeep;

/// Why a cell was refused: text made only on a refusal.
pub type Why = alloc::string::String; // alloc: Cold

/// What the pass did, per cell (index = the cell's index in the input).
#[derive(Debug, PartialEq, Eq)]
pub struct Report {
    /// Bit `ci`: cell `ci` was promoted (at most 64 cells).
    pub promoted: u64,
    /// Why a cell was refused (a use of its root outside a cell access, a
    /// mixed-width access, a store to the root's slot).
    pub refused: IrVec<(usize, Why)>,
    /// Block parameters added.
    pub phis: usize,
    /// Loads deleted (the per-access reads the loop no longer makes).
    pub loads_removed: usize,
    /// Stores deleted.
    pub stores_removed: usize,
    /// Stores inserted (write-backs on exit edges).
    pub stores_added: usize,
    /// The block parameters added: `(block, value, cell index)`, in the function's program. A hint
    /// for `verify::verify_carried`, which checks it rather than trusts it.
    pub params: IrVec<(Block, Value, usize)>,
}

impl Report {
    fn new(m: &Mint) -> Report {
        Report { promoted: 0, refused: m.vec(), phis: 0, loads_removed: 0, stores_removed: 0, stores_added: 0, params: m.vec() }
    }
}

/// Instructions to insert: (block, position in the block's original list, the order deferred, instruction).
type Pending = IrVec<(Block, usize, u32, Instruction)>;

fn defer(pending: &mut Pending, block: Block, pos: usize, inst: Instruction) {
    let seq = pending.len() as u32;
    pending.push((block, pos, seq, inst));
}

#[derive(Clone, Copy)]
enum Access {
    Load { cell: usize, zext: bool },
    Store { cell: usize, value: Value },
}

/// One phi (a block parameter to be).
struct Phi {
    block: Block,
    cell: usize,
    value: Value,
    /// Operand per incoming edge, in `preds[block]` order (filled at seal).
    operands: IrVec<Value>,
    replaced_by: Option<Value>,
}

/// The pass's tables, in the function's program root (`m`): per value, block, cell or phi.
struct Promoter<'a> {
    f: &'a mut Function,
    /// `f`'s program's root: the blocks and edges the pass adds, and its own tables.
    m: Mint,
    cells: &'a [Cell],
    host_state: &'a [StateMask],
    ok: IrVec<bool>,
    /// Per value (index = `Value::index`), the cells whose root the value
    /// resolves to (bit = cell index; at most 64 cells).
    root_bits: IrVec<u64>,
    /// Incoming edges of each block: `(pred, edge index in pred's terminator)`.
    preds: IrVec<IrVec<(Block, usize)>>,
    /// The block at whose start a cell's root pointer is available, and
    /// the instruction index in it after which it is (0: from the start).
    def_block: IrVec<Block>,
    def_pos: IrVec<usize>,
    sealed: IrVec<bool>,
    /// A cell's content at a block's end so far (index = block * cells + cell; sized at `rewrite`).
    current_def: IrVec<Option<Value>>,
    phis: IrVec<Phi>,
    /// The phi each phi value names (`resolve`'s lookup: one per step, not a walk of `phis`).
    phi_of: IrVec<usize>,
    incomplete: IrVec<IrVec<usize>>,
    /// `value -> value` replacements for deleted loads (index = the deleted load's result), and how many.
    replace: IrVec<Option<Value>>,
    nreplace: usize,
    report: Report,
    /// Per block, the cells whose root dominates it (bit = cell index; only there are accesses,
    /// write-backs and reloads; the region is left through split edges).
    reach: IrVec<u64>,
    /// Per block, the cells that may hold an unstored content at its entry (bit = cell index).
    dirty_in: IrVec<u64>,
    /// How deep the current content read walks (`read_variable_recursive` levels, C6 (b)).
    depth: u32,
    max_depth: u32,
}

/// Promote `cells` in `f`. `host_state` is each program host's
/// `HostFlags::state` (the masks decide which calls see the memory). `Err(TooDeep)`: a content read
/// walked more than `max_read_depth` blocks; `f` is then half rewritten (the caller restores it).
pub fn promote(f: &mut Function, cells: &[Cell], host_state: &[StateMask], max_read_depth: u32) -> Result<Report, TooDeep> {
    let m = f.mint();
    if cells.is_empty() {
        return Ok(Report::new(&m));
    }
    assert!(cells.len() <= 64, "promote: at most 64 cells");
    // Blocks unreachable from the entry (a lowerer's never-chained cold
    // copy) are emptied: they would otherwise carry cell accesses the
    // walk never rewrites and feed phis with values from nowhere.
    gut_unreachable(f);
    let nb = f.blocks.len();
    let mut preds: IrVec<IrVec<(Block, usize)>> = m.collect((0..nb).map(|_| m.vec()));
    for (bi, b) in f.blocks.iter().enumerate() {
        for (ei, e) in b.term.edges().enumerate() {
            preds[e.target.index()].push((Block(bi as u32), ei));
        }
    }
    let mut p = Promoter {
        f,
        cells,
        host_state,
        ok: m.vec_filled(true, cells.len()),
        root_bits: m.vec(),
        preds,
        def_block: m.vec(),
        def_pos: m.vec(),
        sealed: m.vec_filled(false, nb),
        current_def: m.vec(),
        phis: m.vec(),
        phi_of: m.vec(),
        incomplete: m.collect((0..nb).map(|_| m.vec())),
        replace: m.vec(),
        nreplace: 0,
        report: Report::new(&m),
        reach: m.vec(),
        dirty_in: m.vec(),
        m,
        depth: 0,
        max_depth: max_read_depth,
    };
    p.check_cells();
    if !p.ok.iter().any(|&o| o) {
        return Ok(p.report);
    }
    p.split_exits();
    p.dataflow();
    p.rewrite()?;
    p.finish();
    Ok(p.report)
}

impl<'a> Promoter<'a> {
    /// The cell index an access `(ptr, offset)` names, if any.
    fn cell_of(&self, ptr: Value, offset: i32) -> Option<usize> {
        let bits = self.bits(ptr);
        if bits == 0 {
            return None;
        }
        (0..self.cells.len()).find(|&ci| bits & (1u64 << ci) != 0 && self.cells[ci].offset == offset)
    }

    /// The cells whose root `v` resolves to (bit = cell index).
    fn bits(&self, v: Value) -> u64 {
        self.root_bits.get(v.index()).copied().unwrap_or(0)
    }

    /// Does `v` resolve to cell `ci`'s root pointer?
    fn is_root(&self, ci: usize, v: Value) -> bool {
        self.root_bits.get(v.index()).is_some_and(|b| b & (1u64 << ci) != 0)
    }

    /// `root_bits`, once, from the definitions.
    fn index_roots(&mut self) {
        let mut bits = self.m.vec_filled(0u64, self.f.values.len());
        for (ci, c) in self.cells.iter().enumerate() {
            match c.root {
                Root::Value(r) => bits[r.index()] |= 1 << ci,
                Root::LoadedFrom { ptr, offset } => {
                    for b in &self.f.blocks {
                        for i in &b.insts {
                            if let (Some(r), Op::Load { ty: Type::Ptr, ptr: p, offset: o, .. }) = (i.result, &i.op) {
                                if *p == ptr && *o == offset {
                                    bits[r.index()] |= 1 << ci;
                                }
                            }
                        }
                    }
                }
            }
        }
        self.root_bits = bits;
    }

    /// Per cell, the block that makes its root available and the
    /// instruction index after which it is (0 for a parameter): the entry
    /// for function parameters and loaded roots, the defining block
    /// otherwise. One walk of the function for every cell (`root_bits`).
    fn def_points(&self) -> IrVec<(Block, usize)> {
        let n = self.cells.len();
        let mut at = self.m.vec_filled((self.f.entry, 0), n);
        let value_root = |ci: usize, v: Value| matches!(self.cells[ci].root, Root::Value(r) if r == v);
        let mut left: u64 = (0..n).filter(|&ci| matches!(self.cells[ci].root, Root::Value(_))).fold(0, |a, ci| a | 1u64 << ci);
        let mut take = |v: Value, point: (Block, usize), left: &mut u64| {
            let bits = self.bits(v) & *left;
            for ci in (0..n).filter(|&ci| bits & (1u64 << ci) != 0 && value_root(ci, v)) {
                at[ci] = point;
                *left &= !(1u64 << ci);
            }
        };
        for (bi, b) in self.f.blocks.iter().enumerate() {
            if left == 0 {
                break;
            }
            for &v in b.params.iter() {
                take(v, (Block(bi as u32), 0), &mut left);
            }
            for (ii, i) in b.insts.iter().enumerate() {
                if let Some(r) = i.result {
                    take(r, (Block(bi as u32), ii + 1), &mut left);
                }
            }
        }
        at
    }

    /// Refuse cells whose memory the function reaches other than through
    /// typed cell accses, and cells whose root is not stable.
    fn check_cells(&mut self) {
        let n = self.cells.len();
        let mut refused: IrVec<Option<Why>> = self.m.vec_filled(None, n);
        self.index_roots();
        let points = self.def_points();
        self.def_block = self.m.collect(points.iter().map(|p| p.0));
        self.def_pos = self.m.collect(points.iter().map(|p| p.1));
        for b in &self.f.blocks {
            for inst in &b.insts {
                // A cell access: the type must be the cell's.
                let (ptr, offset, tyv, zext, stored) = match &inst.op {
                    Op::Load { ty, ptr, offset, .. } => (*ptr, *offset, Some(*ty), false, None),
                    Op::LoadZext { ty, ptr, offset, .. } => (*ptr, *offset, Some(*ty), true, None),
                    Op::Store { ptr, offset, value, .. } => (*ptr, *offset, Some(self.f.value_type(*value)), false, Some(*value)),
                    Op::VecLoad { ptr, offset, .. } | Op::VecStore { ptr, offset, .. } => (*ptr, *offset, None, false, None),
                    Op::Prefetch { .. } => continue,
                    _ => {
                        // Any other use of a root pointer value refuses the root's cells.
                        let mut used = 0u64;
                        op_uses(&inst.op, |v| {
                            used |= self.bits(v);
                            false
                        });
                        for ci in (0..n).filter(|&ci| used & (1u64 << ci) != 0) {
                            refused[ci] = Some(alloc::format!("root used outside a cell access ({:?})", inst.op)); // alloc: Cold
                        }
                        continue;
                    }
                };
                // an access through no root, storing no root, touches no cell
                if self.bits(ptr) | stored.map_or(0, |sv| self.bits(sv)) == 0 {
                    if stored.is_some() {
                        self.slot_stored(ptr, offset, &mut refused);
                    }
                    continue;
                }
                for ci in 0..n {
                    let c = self.cells[ci];
                    // A store through a root pointer as the *value* is a use too.
                    if let Some(sv) = stored {
                        if self.is_root(ci, sv) {
                            refused[ci] = Some(Why::from("root pointer stored to memory")); // alloc: Cold
                        }
                    }
                    if !self.is_root(ci, ptr) {
                        continue;
                    }
                    if offset != c.offset {
                        // Another cell of the same root, or an unmarked slot: the
                        // width may overlap ours.
                        let w = tyv.map_or(16, |t| t.bytes() as i32);
                        if offset < c.offset + c.ty.bytes() as i32 && c.offset < offset + w {
                            refused[ci] = Some(alloc::format!("overlapping access at offset {offset}")); // alloc: Cold
                        }
                        continue;
                    }
                    match tyv {
                        Some(t) if t == c.ty && (!zext || t.is_int()) => {}
                        Some(t) => refused[ci] = Some(alloc::format!("access of type {t:?} to a {:?} cell", c.ty)), // alloc: Cold
                        None => refused[ci] = Some(Why::from("vector access to the cell")), // alloc: Cold
                    }
                }
                // A loaded root's slot must never be stored.
                if stored.is_some() {
                    self.slot_stored(ptr, offset, &mut refused);
                }
            }
            // Terminators: a root passed as a block argument or returned.
            let passed = b.term.edges().flat_map(|e| e.args.iter()).chain(match &b.term {
                Terminator::Return(vs) => vs.iter(),
                _ => [].iter(),
            });
            if passed.fold(0u64, |a, &v| a | self.bits(v)) == 0 {
                continue;
            }
            for ci in 0..n {
                let uses_root = b.term.edges().any(|e| e.args.iter().any(|&a| self.is_root(ci, a)))
                    || matches!(&b.term, Terminator::Return(vs) if vs.iter().any(|&a| self.is_root(ci, a)));
                if uses_root {
                    if let Root::Value(_) = self.cells[ci].root {
                        refused[ci] = Some(Why::from("root passed on an edge or returned")); // alloc: Cold
                    }
                }
            }
        }
        for ci in 0..n {
            if let Some(why) = refused[ci].take() {
                self.ok[ci] = false;
                self.report.refused.push((ci, why)); // at most 64 (one per cell)
            }
        }
    }

    /// What an instruction does to the cells.
    /// A store to `ptr + offset`: refuses the cells whose root is loaded from that slot.
    fn slot_stored(&self, ptr: Value, offset: i32, refused: &mut [Option<Why>]) {
        for ci in 0..self.cells.len() {
            if let Root::LoadedFrom { ptr: rp, offset: ro } = self.cells[ci].root {
                if ptr == rp && offset == ro {
                    refused[ci] = Some(Why::from("the root's own slot is stored")); // alloc: Cold
                }
            }
        }
    }

    fn access(&self, op: &Op) -> Option<Access> {
        match op {
            Op::Load { ptr, offset, .. } => self.cell_of(*ptr, *offset).filter(|&c| self.ok[c]).map(|cell| Access::Load { cell, zext: false }),
            Op::LoadZext { ptr, offset, .. } => self.cell_of(*ptr, *offset).filter(|&c| self.ok[c]).map(|cell| Access::Load { cell, zext: true }),
            Op::Store { ptr, offset, value, .. } => self.cell_of(*ptr, *offset).filter(|&c| self.ok[c]).map(|cell| Access::Store { cell, value: *value }),
            _ => None,
        }
    }

    /// The classes a call touches: `None` for a transparent one.
    fn call_mask(&self, op: &Op) -> Option<StateMask> {
        match op {
            Op::CallHost { callee, .. } => {
                let m = self.host_state[callee.index()];
                if m == StateMask::NONE {
                    None
                } else {
                    Some(m)
                }
            }
            Op::Call { .. } => Some(StateMask::ALL),
            _ => None,
        }
    }

    /// The blocks each cell's root dominates (`reach`), and the split
    /// edges: an edge from a dominated block to one outside the region
    /// gets a block of its own, where the cell is written back (a shared
    /// return block reached from before the root's definition can hold
    /// no store through the root). Roots defined in the entry dominate
    /// everything and split nothing.
    fn split_exits(&mut self) {
        let n = self.cells.len();
        let idom = idoms(self.f, &self.m);
        let order = rpo(self.f, &self.m);
        let nb = self.f.blocks.len();
        // the blocks `d` dominates: `d`, then in reverse post-order each block whose immediate dominator is (an immediate
        // dominator comes first); one walk per distinct root block, not one idom chain per (cell, block)
        let m = &self.m;
        let dominated = |d: Block| -> IrVec<bool> {
            let mut dom = m.vec_filled(false, nb);
            dom[d.index()] = true;
            for &b in order.iter() {
                if let Some(p) = idom[b.index()].filter(|&p| p != b) {
                    dom[b.index()] |= dom[p.index()];
                }
            }
            dom
        };
        let mut reach = m.vec_filled(0u64, nb);
        let mut by_root: IrVec<(Block, IrVec<bool>)> = m.vec();
        for ci in (0..n).filter(|&ci| self.ok[ci]) {
            let d = self.def_block[ci];
            if !by_root.iter().any(|r| r.0 == d) {
                by_root.push((d, dominated(d)));
            }
            let dom = &by_root.iter().find(|r| r.0 == d).expect("root block").1;
            for bi in 0..nb {
                reach[bi] |= u64::from(dom[bi]) << ci;
            }
        }
        // Split: for every edge p -> s with some cell dominated at p and
        // not at s.
        let mut splits: IrVec<(Block, usize)> = m.vec();
        for bi in 0..nb {
            for (ei, e) in self.f.blocks[bi].term.edges().enumerate() {
                if reach[bi] & !reach[e.target.index()] != 0 {
                    splits.push((Block(bi as u32), ei));
                }
            }
        }
        drop(by_root);
        for &(p, ei) in splits.iter() {
            let nbk = Block(self.f.blocks.len() as u32);
            let e = core::mem::replace(edge_mut(&mut self.f.blocks[p.index()].term, ei), self.m.edge(nbk, &[]));
            self.f.blocks.push(BasicBlock { params: self.m.slice_from(&[]), insts: self.m.vec(), term: Terminator::Br(e) });
            // The split block is dominated by what dominates p.
            reach.push(reach[p.index()]);
            self.preds.push(self.m.vec_from(&[(p, ei)]));
            self.sealed.push(false);
            self.incomplete.push(self.m.vec());
            let s = self.f.blocks[nbk.index()].term.edges().next().expect("split edge").target;
            let sp = &mut self.preds[s.index()];
            if let Some(k) = sp.iter().position(|&(q, qi)| q == p && qi == ei) {
                sp[k] = (nbk, 0);
            }
            if self.f.hints.cold_blocks.contains(&p) || self.f.hints.cold_blocks.contains(&s) {
                self.f.hints.cold_blocks.push(nbk);
            }
        }
        self.reach = reach;
    }

    /// Forward dataflow: `dirty_in` (an unstored content may be live at
    /// block entry).
    fn dataflow(&mut self) {
        let nb = self.f.blocks.len();
        let n = self.cells.len();
        let mut dirty_in = self.m.vec_filled(0u64, nb);
        // reverse post-order: a forward problem settles in a few sweeps (the loops' depth), where block order may
        // take one sweep per block of a long chain; the fixpoint is the same (unreachable blocks are empty)
        let order = rpo(self.f, &self.m);
        let mut changed = true;
        while changed {
            changed = false;
            for &b in order.iter() {
                let bi = b.index();
                let mut d = dirty_in[bi];
                for inst in self.f.blocks[bi].insts.iter() {
                    if let Some(a) = self.access(&inst.op) {
                        if let Access::Store { cell, .. } = a {
                            d |= 1 << cell;
                        }
                    } else if let Some(m) = self.call_mask(&inst.op) {
                        for ci in 0..n {
                            if m.contains(self.cells[ci].class) {
                                d &= !(1 << ci);
                            }
                        }
                    }
                }
                // A block leaving a cell's region writes it back before
                // its terminator (a split block, or a return).
                for e in self.f.blocks[bi].term.edges() {
                    let t = e.target.index();
                    let leaves = self.reach[bi] & !self.reach[t];
                    let gains = d & !leaves & !dirty_in[t];
                    if gains != 0 {
                        dirty_in[t] |= gains;
                        changed = true;
                    }
                }
            }
        }
        self.dirty_in = dirty_in;
    }

    fn new_value(&mut self, ty: Type) -> Value {
        let v = Value(self.f.values.len() as u32);
        self.f.values.push(ty);
        v
    }

    fn resolve(&self, mut v: Value) -> Value {
        for _ in 0..self.phis.len() + 1 {
            let next = self.phi_of.get(v.index()).filter(|&&pi| pi != usize::MAX).and_then(|&pi| self.phis[pi].replaced_by);
            match next {
                Some(n) if n != v => v = n,
                _ => return v,
            }
        }
        v
    }

    /// The final value standing for `v`: through the phi replacements and
    /// the deleted loads' stand-ins, to a fixpoint.
    fn canon(&self, mut v: Value) -> Value {
        for _ in 0..self.phis.len() + self.nreplace + 1 {
            let r = self.resolve(v);
            match self.replace.get(r.index()).copied().flatten() {
                Some(n) if n != r => v = n,
                _ => return r,
            }
        }
        v
    }

    /// The deleted load `r` stands for `v`.
    fn set_replace(&mut self, r: Value, v: Value) {
        if self.replace.len() <= r.index() {
            self.replace.resize(r.index() + 1, None);
        }
        if self.replace[r.index()].replace(v).is_none() {
            self.nreplace += 1;
        }
    }

    fn new_phi(&mut self, block: Block, cell: usize) -> usize {
        let v = self.new_value(self.cells[cell].ty);
        let operands = self.m.vec();
        self.phis.push(Phi { block, cell, value: v, operands, replaced_by: None });
        if self.phi_of.len() <= v.index() {
            self.phi_of.resize(v.index() + 1, usize::MAX);
        }
        self.phi_of[v.index()] = self.phis.len() - 1;
        self.phis.len() - 1
    }

    fn write_variable(&mut self, cell: usize, block: Block, v: Value) {
        let i = block.index() * self.cells.len() + cell;
        self.current_def[i] = Some(v);
    }

    /// The cell's content at the end of `block` (or, while `block` is
    /// being rewritten, at the current point of it).
    fn read_variable(&mut self, cell: usize, block: Block, pending: &mut Pending) -> Result<Value, TooDeep> {
        if let Some(v) = self.current_def[block.index() * self.cells.len() + cell] {
            return Ok(v);
        }
        if self.depth >= self.max_depth {
            return Err(TooDeep);
        }
        self.depth += 1;
        let v = self.read_variable_recursive(cell, block, pending);
        self.depth -= 1;
        v
    }

    fn read_variable_recursive(&mut self, cell: usize, block: Block, pending: &mut Pending) -> Result<Value, TooDeep> {
        let v = if self.def_block[cell] == block {
            // The root becomes available here: the initial content is a
            // load at the block's start (`pending` gets it; the
            // rewriter places it after the parameters).
            let ty = self.cells[cell].ty;
            let r = self.new_value(ty);
            let pos = self.def_pos[cell];
            let ptr = self.root_ptr_at(cell, pending, block, pos);
            defer(pending, block, pos, Instruction { result: Some(r), op: Op::Load { ty, ptr, offset: self.cells[cell].offset, unaligned: false } });
            r
        } else if !self.sealed[block.index()] {
            let ph = self.new_phi(block, cell);
            self.incomplete[block.index()].push(ph);
            self.phis[ph].value
        } else if self.preds[block.index()].len() == 1 {
            let (p, _) = self.preds[block.index()][0];
            self.read_variable(cell, p, pending)?
        } else if self.preds[block.index()].is_empty() {
            // Unreachable from the root's definition: never read there.
            let ty = self.cells[cell].ty;
            let r = self.new_value(ty);
            let ptr = self.root_ptr_at(cell, pending, block, 0);
            defer(pending, block, 0, Instruction { result: Some(r), op: Op::Load { ty, ptr, offset: self.cells[cell].offset, unaligned: false } });
            r
        } else {
            let ph = self.new_phi(block, cell);
            let v = self.phis[ph].value;
            self.write_variable(cell, block, v);
            self.add_phi_operands(ph, pending)?;
            v
        };
        self.write_variable(cell, block, v);
        Ok(v)
    }

    fn add_phi_operands(&mut self, ph: usize, pending: &mut Pending) -> Result<(), TooDeep> {
        let block = self.phis[ph].block;
        let cell = self.phis[ph].cell;
        let np = self.preds[block.index()].len();
        let mut ops = self.m.vec_with_capacity(np);
        for k in 0..np {
            let (p, _) = self.preds[block.index()][k];
            ops.push(self.read_variable(cell, p, pending)?);
        }
        self.phis[ph].operands = ops;
        Ok(())
    }

    /// The root pointer for a store / reload at a point of `block`: the
    /// root value itself, or a fresh load of the slot (`pending` gets it
    /// at position `pos`).
    fn root_ptr_at(&mut self, cell: usize, pending: &mut Pending, block: Block, pos: usize) -> Value {
        match self.cells[cell].root {
            Root::Value(v) => v,
            Root::LoadedFrom { ptr, offset } => {
                let r = self.new_value(Type::Ptr);
                defer(pending, block, pos, Instruction { result: Some(r), op: Op::Load { ty: Type::Ptr, ptr, offset, unaligned: false } });
                r
            }
        }
    }

    /// Walk the blocks in reverse post-order, rewriting each: deleted
    /// accesses, inserted write-backs and reloads, phis at joins.
    fn rewrite(&mut self) -> Result<(), TooDeep> {
        let order = rpo(self.f, &self.m);
        let nb = self.f.blocks.len();
        let n = self.cells.len();
        // Blocks whose predecessors are all processed are sealed on
        // arrival; the rest (loop headers) at the end.
        let mut done = self.m.vec_filled(false, nb);
        // Instructions to insert: (block, position in the *original*
        // list, instruction); a position of `usize::MAX` means before the
        // terminator. Applied per block after its walk.
        let mut pending: Pending = self.m.vec();
        let m = &self.m;
        let mut deleted: IrVec<IrVec<bool>> = m.collect(self.f.blocks.iter().map(|b| m.vec_filled(false, b.insts.len())));
        self.current_def = self.m.vec_filled(None, nb * n);
        for &b in order.iter() {
            let bi = b.index();
            if self.preds[bi].iter().all(|(p, _)| done[p.index()]) {
                self.seal(b, &mut pending)?;
            }
            let mut dirty = self.dirty_in[bi];
            let ninst = self.f.blocks[bi].insts.len();
            for ii in 0..ninst {
                let (acc, mask, result) = {
                    let inst = &self.f.blocks[bi].insts[ii];
                    (self.access(&inst.op), self.call_mask(&inst.op), inst.result)
                };
                if let Some(a) = acc {
                    match a {
                        Access::Load { cell, zext } => {
                            let content = self.read_variable(cell, b, &mut pending)?;
                            let r = result.expect("load result");
                            if zext && self.cells[cell].ty != Type::I64 {
                                let z = self.new_value(Type::I64);
                                defer(&mut pending, b, ii, Instruction { result: Some(z), op: Op::Conv { op: ConvOp::Zext, to: Type::I64, a: content } });
                                self.set_replace(r, z);
                            } else {
                                self.set_replace(r, content);
                            }
                            deleted[bi][ii] = true;
                            self.report.loads_removed += 1;
                        }
                        Access::Store { cell, value } => {
                            self.write_variable(cell, b, value);
                            dirty |= 1 << cell;
                            deleted[bi][ii] = true;
                            self.report.stores_removed += 1;
                        }
                    }
                } else if let Some(m) = mask {
                    for ci in 0..n {
                        if !self.ok[ci] || !m.contains(self.cells[ci].class) || self.reach[bi] & 1 << ci == 0 {
                            continue;
                        }
                        if dirty & 1 << ci != 0 {
                            let content = self.read_variable(ci, b, &mut pending)?;
                            let ptr = self.root_ptr_at(ci, &mut pending, b, ii);
                            defer(&mut pending, b, ii, Instruction { result: None, op: Op::Store { ptr, offset: self.cells[ci].offset, value: content, unaligned: false } });
                            self.report.stores_added += 1;
                            dirty &= !(1 << ci);
                        }
                        // Reload after the call (the host may have written it).
                        let ty = self.cells[ci].ty;
                        let r = self.new_value(ty);
                        let ptr = self.root_ptr_at(ci, &mut pending, b, ii + 1);
                        defer(&mut pending, b, ii + 1, Instruction { result: Some(r), op: Op::Load { ty, ptr, offset: self.cells[ci].offset, unaligned: false } });
                        self.write_variable(ci, b, r);
                    }
                }
            }
            let leaving = self.f.blocks[bi].term.edges().fold(0u64, |a, e| a | self.reach[bi] & !self.reach[e.target.index()]);
            let returns = matches!(self.f.blocks[bi].term, Terminator::Return(_));
            {
                for ci in 0..n {
                    if self.ok[ci] && self.reach[bi] & dirty & 1 << ci != 0 && (returns || leaving & 1 << ci != 0) {
                        let content = self.read_variable(ci, b, &mut pending)?;
                        let ptr = self.root_ptr_at(ci, &mut pending, b, usize::MAX);
                        defer(&mut pending, b, usize::MAX, Instruction { result: None, op: Op::Store { ptr, offset: self.cells[ci].offset, value: content, unaligned: false } });
                        self.report.stores_added += 1;
                    }
                }
            }
            done[bi] = true;
        }
        for bi in 0..nb {
            if !self.sealed[bi] {
                self.seal(Block(bi as u32), &mut pending)?;
            }
        }
        // Trivial phis: a fixpoint over the resolved operands.
        loop {
            let mut changed = false;
            for pi in 0..self.phis.len() {
                if self.phis[pi].replaced_by.is_some() {
                    continue;
                }
                let me = self.phis[pi].value;
                let mut same: Option<Value> = None;
                let mut trivial = true;
                for k in 0..self.phis[pi].operands.len() {
                    let o = self.canon(self.phis[pi].operands[k]);
                    if o == me || Some(o) == same {
                        continue;
                    }
                    if same.is_some() {
                        trivial = false;
                        break;
                    }
                    same = Some(o);
                }
                if trivial {
                    if let Some(s) = same {
                        self.phis[pi].replaced_by = Some(s);
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        // Materialize: params and edge arguments for the surviving phis,
        // in cell declaration order within a block (the hottest cell was
        // declared first; the pinned budget fills in parameter order).
        let mut order = self.m.collect((0..self.phis.len()).filter(|&pi| self.phis[pi].replaced_by.is_none()));
        order.sort_unstable_by_key(|&pi| (self.phis[pi].block.0, self.phis[pi].cell, pi));
        for &pi in order.iter() {
            let b = self.phis[pi].block;
            let v = self.phis[pi].value;
            let params = self.m.appended(&self.f.blocks[b.index()].params, v);
            self.f.blocks[b.index()].params = params;
            self.report.params.push((b, v, self.phis[pi].cell));
            for k in 0..self.preds[b.index()].len() {
                let (p, ei) = self.preds[b.index()][k];
                let arg = self.canon(self.phis[pi].operands[k]);
                let e = edge_mut(&mut self.f.blocks[p.index()].term, ei);
                e.args = self.m.appended(&e.args, arg);
            }
            self.report.phis += 1;
        }
        // Apply the deletions and insertions per block (deferral order within a position).
        pending.sort_unstable_by_key(|&(b, pos, seq, _)| (b.0, pos, seq));
        let mut pending = pending.into_iter().map(|(b, pos, _, inst)| (b, pos, inst)).peekable();
        for bi in 0..nb {
            let old = core::mem::replace(&mut self.f.blocks[bi].insts, self.m.vec());
            let mut new = self.m.vec_with_capacity(old.len() + 4);
            let mut at = |pos: usize, new: &mut IrVec<Instruction>| {
                while let Some(p) = pending.next_if(|p| p.0.index() == bi && p.1 <= pos) {
                    new.push(p.2);
                }
            };
            for (ii, inst) in old.into_iter().enumerate() {
                at(ii, &mut new);
                if !deleted[bi][ii] {
                    new.push(inst);
                }
            }
            at(usize::MAX, &mut new);
            self.f.blocks[bi].insts = new;
        }
        for ci in 0..n {
            self.report.promoted |= (self.ok[ci] as u64) << ci;
        }
        Ok(())
    }

    fn seal(&mut self, b: Block, pending: &mut Pending) -> Result<(), TooDeep> {
        self.sealed[b.index()] = true;
        let inc = core::mem::replace(&mut self.incomplete[b.index()], self.m.vec());
        for &ph in inc.iter() {
            self.add_phi_operands(ph, pending)?;
        }
        Ok(())
    }

    /// Replace the deleted loads' results (through the phi replacements),
    /// then drop the pure instructions nothing reads and the hints that
    /// named them.
    fn finish(&mut self) {
        let mut map: IrVec<Option<Value>> = self.m.vec_filled(None, self.f.values.len());
        for k in (0..self.replace.len()).filter(|&k| self.replace[k].is_some()) {
            map[k] = Some(self.canon(Value(k as u32)));
        }
        for ph in self.phis.iter() {
            if ph.replaced_by.is_some() {
                map[ph.value.index()] = Some(self.canon(ph.value));
            }
        }
        let mut sub = |v: &mut Value| {
            if let Some(n) = map.get(v.index()).copied().flatten() {
                *v = n;
            }
        };
        for b in self.f.blocks.iter_mut() {
            for inst in b.insts.iter_mut() {
                op_values_mut(&mut inst.op, &mut sub);
            }
            term_values_mut(&mut b.term, &mut sub);
        }
        // Hints naming a deleted load.
        let mut defined = self.m.vec_filled(false, self.f.values.len());
        for b in self.f.blocks.iter() {
            for &p in b.params.iter() {
                defined[p.index()] = true;
            }
            for i in b.insts.iter() {
                if let Some(r) = i.result {
                    defined[r.index()] = true;
                }
            }
        }
        self.f.hints.remat.retain(|v| defined[v.index()]);
        self.f.hints.nowrap.retain(|v| defined[v.index()]);
        dce(self.f);
    }
}

/// Dead pure instructions removed (a fixpoint: a dead load frees its
/// pointer load), the `remat` / `nowrap` hints kept consistent.
pub fn dce(f: &mut Function) {
    let m = f.mint();
    loop {
        let mut used = m.vec_filled(false, f.values.len());
        for b in f.blocks.iter_mut() {
            for inst in b.insts.iter_mut() {
                op_values_mut(&mut inst.op, &mut |v: &mut Value| used[v.index()] = true);
            }
            term_values_mut(&mut b.term, &mut |v: &mut Value| used[v.index()] = true);
        }
        let mut removed: IrVec<Value> = m.vec();
        for b in f.blocks.iter_mut() {
            b.insts.retain(|inst| match inst.result {
                Some(r) if !used[r.index()] && pure(&inst.op) => {
                    removed.push(r);
                    false
                }
                _ => true,
            });
        }
        if removed.is_empty() {
            break;
        }
        f.hints.remat.retain(|v| !removed.contains(v));
        f.hints.nowrap.retain(|v| !removed.contains(v));
    }
}

pub(crate) fn pure(op: &Op) -> bool {
    matches!(
        op,
        Op::Const { .. }
            | Op::IntBin { .. }
            | Op::IntOvf { .. }
            | Op::IntUn { .. }
            | Op::Icmp { .. }
            | Op::FloatBin { .. }
            | Op::FloatUn { .. }
            | Op::Fcmp { .. }
            | Op::Conv { .. }
            | Op::Select { .. }
            | Op::Load { .. }
            | Op::LoadZext { .. }
            | Op::StackAddr { .. }
            | Op::PtrAdd { .. }
            | Op::Crc32c { .. }
            | Op::AddHi { .. }
            | Op::VecLoad { .. }
            | Op::VecBin { .. }
            | Op::VecShift { .. }
            | Op::VecCmp { .. }
            | Op::VecSelect { .. }
            | Op::VecReduceAdd { .. }
            | Op::VecMovmask { .. }
            | Op::VecSplat { .. }
            | Op::VecExtract { .. }
            | Op::VecInsert { .. }
            | Op::VecWiden { .. }
            | Op::VecNarrow { .. }
    )
}

/// Empty every block unreachable from the entry (no instructions, an
/// `Unreachable` terminator; the parameters stay so value ids are
/// stable). The `remat` / `nowrap` hints naming their values are dropped.
pub fn gut_unreachable(f: &mut Function) {
    let m = f.mint();
    let order = rpo(f, &m);
    let mut live = m.vec_filled(false, f.blocks.len());
    for &b in order.iter() {
        live[b.index()] = true;
    }
    let mut gone: IrVec<Value> = m.vec();
    for (bi, b) in f.blocks.iter_mut().enumerate() {
        if !live[bi] {
            for i in b.insts.iter() {
                if let Some(r) = i.result {
                    gone.push(r);
                }
            }
            b.insts.clear();
            b.term = Terminator::Unreachable;
        }
    }
    if !gone.is_empty() {
        f.hints.remat.retain(|v| !gone.contains(v));
        f.hints.nowrap.retain(|v| !gone.contains(v));
    }
}

/// Immediate dominators (Cooper, Harvey, Kennedy), `None` for the entry
/// and for unreachable blocks; in `m`'s program (`f`'s).
pub fn idoms(f: &Function, m: &Mint) -> IrVec<Option<Block>> {
    let nb = f.blocks.len();
    let order = rpo(f, m);
    let mut rpo_index = m.vec_filled(usize::MAX, nb);
    for (i, &b) in order.iter().enumerate() {
        rpo_index[b.index()] = i;
    }
    let mut preds: IrVec<IrVec<Block>> = m.collect((0..nb).map(|_| m.vec()));
    for (bi, b) in f.blocks.iter().enumerate() {
        for e in b.term.edges() {
            preds[e.target.index()].push(Block(bi as u32));
        }
    }
    let mut idom = m.vec_filled(None, nb);
    idom[f.entry.index()] = Some(f.entry);
    let intersect = |idom: &[Option<Block>], mut a: Block, mut b: Block| -> Block {
        while a != b {
            while rpo_index[a.index()] > rpo_index[b.index()] {
                a = idom[a.index()].expect("processed");
            }
            while rpo_index[b.index()] > rpo_index[a.index()] {
                b = idom[b.index()].expect("processed");
            }
        }
        a
    };
    let mut changed = true;
    while changed {
        changed = false;
        for &b in order.iter().skip(1) {
            let mut new_idom: Option<Block> = None;
            for &p in preds[b.index()].iter() {
                if idom[p.index()].is_none() {
                    continue;
                }
                new_idom = Some(match new_idom {
                    None => p,
                    Some(q) => intersect(&idom, p, q),
                });
            }
            if new_idom != idom[b.index()] {
                idom[b.index()] = new_idom;
                changed = true;
            }
        }
    }
    idom[f.entry.index()] = None;
    idom
}

/// Reverse post-order from the entry, in `m`'s program (`f`'s).
pub fn rpo(f: &Function, m: &Mint) -> IrVec<Block> {
    let nb = f.blocks.len();
    let mut seen = m.vec_filled(false, nb);
    let mut post = m.vec_with_capacity(nb);
    // Iterative DFS.
    let mut stack: IrVec<(Block, usize)> = m.vec_from(&[(f.entry, 0)]);
    seen[f.entry.index()] = true;
    while let Some((b, i)) = stack.last_mut() {
        if let Some(s) = f.blocks[b.index()].term.edges().nth(*i).map(|e| e.target) {
            *i += 1;
            if !seen[s.index()] {
                seen[s.index()] = true;
                stack.push((s, 0));
            }
        } else {
            post.push(*b);
            stack.pop();
        }
    }
    post.reverse();
    post
}

pub(crate) fn edge_mut(t: &mut Terminator, ei: usize) -> &mut Edge {
    match t {
        Terminator::Br(e) => e,
        Terminator::CondBr { then_edge, else_edge, .. } => {
            if ei == 0 {
                then_edge
            } else {
                else_edge
            }
        }
        Terminator::Switch { cases, default, .. } => {
            if ei < cases.len() {
                &mut cases[ei].edge
            } else {
                default
            }
        }
        Terminator::Return(_) | Terminator::Unreachable => unreachable!("edge of a returning terminator"),
    }
}

/// Visit the values an op reads; stops early when `f` returns true.
pub(crate) fn op_uses(op: &Op, mut f: impl FnMut(Value) -> bool) -> bool {
    let mut hit = false;
    op.for_each_operand(|v| hit = hit || f(v));
    hit
}

/// Every value operand of an op, mutably.
pub(crate) fn op_values_mut(op: &mut Op, f: &mut impl FnMut(&mut Value)) {
    match op {
        Op::Const { .. } | Op::StackAddr { .. } => {}
        Op::IntBin { a, b, .. } | Op::IntOvf { a, b, .. } | Op::Icmp { a, b, .. } | Op::FloatBin { a, b, .. } | Op::Fcmp { a, b, .. } | Op::VecBin { a, b, .. } | Op::VecCmp { a, b, .. } | Op::VecNarrow { a, b } => {
            f(a);
            f(b);
        }
        Op::IntUn { a, .. } | Op::FloatUn { a, .. } | Op::Conv { a, .. } | Op::VecShift { a, .. } | Op::VecReduceAdd { a } | Op::VecMovmask { a } | Op::VecSplat { a, .. } | Op::VecExtract { a, .. } | Op::VecWiden { a, .. } => f(a),
        Op::Select { cond, a, b } => {
            f(cond);
            f(a);
            f(b);
        }
        Op::VecSelect { mask, a, b } => {
            f(mask);
            f(a);
            f(b);
        }
        Op::VecInsert { a, value, .. } => {
            f(a);
            f(value);
        }
        Op::Load { ptr, .. } | Op::LoadZext { ptr, .. } | Op::Prefetch { ptr, .. } | Op::VecLoad { ptr, .. } => f(ptr),
        Op::Store { ptr, value, .. } | Op::VecStore { ptr, value, .. } => {
            f(ptr);
            f(value);
        }
        Op::PtrAdd { base, index, .. } => {
            f(base);
            f(index);
        }
        Op::Call { args, .. } | Op::CallHost { args, .. } => {
            for a in args.iter_mut() {
                f(a);
            }
        }
        Op::Crc32c { acc, data } => {
            f(acc);
            f(data);
        }
        Op::AddHi { alo, blo, ahi, bhi } => {
            f(alo);
            f(blo);
            f(ahi);
            f(bhi);
        }
    }
}

pub(crate) fn term_values_mut(t: &mut Terminator, f: &mut impl FnMut(&mut Value)) {
    match t {
        Terminator::Br(e) => e.args.iter_mut().for_each(|v| f(v)),
        Terminator::CondBr { cond, then_edge, else_edge } => {
            f(cond);
            then_edge.args.iter_mut().for_each(|v| f(v));
            else_edge.args.iter_mut().for_each(|v| f(v));
        }
        Terminator::Switch { value, cases, default } => {
            f(value);
            for c in cases.iter_mut() {
                c.edge.args.iter_mut().for_each(|v| f(v));
            }
            default.args.iter_mut().for_each(|v| f(v));
        }
        Terminator::Return(vs) => vs.iter_mut().for_each(|v| f(v)),
        Terminator::Unreachable => {}
    }
}

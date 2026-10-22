//! Construction API. The lowerer and the text loader both go through this.
//!
//! `FunctionBuilder` appends to one block at a time. Blocks can be created
//! ahead of use (for forward branches) and filled later. Each instruction
//! method returns the `Value` it defines.

use crate::*;
use alloc::vec::Vec;

pub struct ProgramBuilder {
    /// The function builder's tables, in the program's root: declared first, so they drop before it.
    scratch: Scratch,
    program: Program,
    /// The lines the lowerer attributed to a tag (`FunctionBuilder::tag_lines`); kept out of the `Program` (its content is
    /// the code key, and a tag is the statement's, not the IR's).
    tagged: Vec<(i32, u32)>,
    /// The JIT map's tags (`jit_tags`), `None` unless asked for.
    jit: Option<crate::jit_tags::JitState>,
}

impl Default for ProgramBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ProgramBuilder {
    pub fn new() -> Self {
        Self::of(Program::new())
    }

    /// A program whose root is `owner`'s from its birth (a lowering's: its statement).
    pub fn new_owned(owner: memledger::Owner) -> Self {
        Self::of(Program::new_owned(owner))
    }

    fn of(mut program: Program) -> Self {
        let m = program.mint();
        Self { scratch: Scratch { consts: m.vec(), marks: HintMarks::new(&m) }, program, tagged: Vec::new(), jit: None }
    }

    pub fn declare_host(&mut self, name: &str, params: &[Type], ret: Option<Type>, addr: usize, flags: HostFlags) -> HostFn {
        let id = HostFn(self.program.host_fns.len() as u32);
        let m = self.program.mint();
        self.program.host_fns.push(HostFnDecl { name: m.str(name), params: m.vec_from(params), ret, addr, flags });
        id
    }

    /// Reserve a function id with its signature so it can be called before its
    /// body is built. The body is supplied with `define`.
    pub fn declare_func(&mut self, name: &str, params: &[Type], returns: &[Type]) -> Func {
        let id = Func(self.program.funcs.len() as u32);
        let f = self.program.mint().function(name, params, returns);
        self.program.funcs.push(f);
        id
    }

    /// Start building the body of a declared function. The entry block is
    /// created with the function's parameters as its block parameters.
    pub fn define(&mut self, func: Func) -> FunctionBuilder<'_> {
        let mint = self.program.mint();
        let Program { funcs, host_fns, .. } = &mut self.program;
        let f = &mut funcs[func.index()];
        assert!(f.blocks.is_empty(), "function {} already defined", f.name);
        let tagged = &mut self.tagged;
        let jit = self.jit.as_mut();
        if let Some(j) = jit {
            j.define(func.0);
        }
        let jit = self.jit.as_mut();
        let Scratch { consts, marks } = &mut self.scratch;
        consts.clear();
        marks.clear();
        let mut fb = FunctionBuilder { func: f, host_fns, tagged, jit, mint, current: None, consts, marks, pushed: 0, filled: 0 };
        let params = fb.mint.vec_from(&fb.func.params);
        let entry = fb.create_block(&params);
        fb.func.entry = entry;
        fb.switch_to(entry);
        fb
    }

    pub fn program(&self) -> &Program {
        &self.program
    }

    pub fn finish(self) -> Program {
        self.program
    }

    /// The lines each tag was given (`FunctionBuilder::tag_lines`, summed per tag in first-tagged order), taken.
    pub fn take_tagged(&mut self) -> Vec<(i32, u32)> {
        core::mem::take(&mut self.tagged)
    }

    /// Record the JIT map's tags from here on (`FunctionBuilder::set_tag`; gravity_backend perfmap mode `tags`).
    pub fn jit_tags(&mut self) {
        let owner = self.program.owner();
        self.jit.get_or_insert_with(|| crate::jit_tags::JitState::new(owner));
    }

    /// The JIT map's tags recorded so far, taken (`None` unless `jit_tags`).
    pub fn take_jit_tags(&mut self) -> Option<crate::JitTags> {
        self.jit.take().map(crate::jit_tags::JitState::finish)
    }
}

/// A block's first reservation: the mean instructions per filled block of the function so far, as many as fill the root's
/// chunk for them (AllocSet: a power of two in bytes, here 128 B..2 KiB). A power-of-two count of 40 B instructions left up
/// to 37 % of its chunk empty (4 = 160 B in a 256 B chunk), the lists spread and the view build walked them slower (D4463).
/// A block past it grows by doubling, which keeps filling its chunk.
fn block_reserve(pushed: u32, filled: u32) -> usize {
    let size = core::mem::size_of::<Instruction>();
    let mean = (pushed as usize / (filled as usize).max(1)).max(1);
    (mean * size).next_power_of_two().clamp(128, 2048) / size
}

pub struct FunctionBuilder<'a> {
    func: &'a mut Function,
    /// The program's host declarations: a lowering that finds a host at a
    /// site (a direct-call shim of a cataloged builtin) declares it here
    /// instead of every program declaring every possible one up front.
    host_fns: &'a mut IrVec<HostFnDecl>,
    /// The program's tagged lines (`ProgramBuilder::tagged`).
    tagged: &'a mut Vec<(i32, u32)>,
    /// The JIT map's tags (`ProgramBuilder::jit_tags`), `None` when off: one branch per pushed instruction.
    jit: Option<&'a mut crate::jit_tags::JitState>,
    /// The program's root: every list the body gains.
    mint: Mint,
    current: Option<Block>,
    /// Constants by value index, for folding at build time. Dense (values are numbered in creation order): D3886, the
    /// BTreeMap it replaces cost a search per folded op (`int_bin`, `conv`, `select`) and a node per constant, ~11% of a
    /// lowering's cycles.
    consts: &'a mut IrVec<Option<(Type, u64)>>,
    /// The blocks and values each hint list holds (D3698).
    marks: &'a mut HintMarks,
    /// Instructions pushed and blocks that have one, for a block's first reservation (`push`).
    pushed: u32,
    filled: u32,
}

/// D3698: which blocks / values each of the function's hint lists holds, so a mark tests membership in one read: a scan
/// of the list per mark was quadratic in a function's cold blocks, loop headers, likely edges and checked values (a join
/// chain's probe pipeline: a thousand of each). The lists keep their order and contents.
struct HintMarks {
    cold: IrVec<bool>,
    header: IrVec<bool>,
    likely: IrVec<bool>,
    freq: IrVec<bool>,
    remat: IrVec<bool>,
    nowrap: IrVec<bool>,
}

impl HintMarks {
    fn new(m: &Mint) -> HintMarks {
        HintMarks { cold: m.vec(), header: m.vec(), likely: m.vec(), freq: m.vec(), remat: m.vec(), nowrap: m.vec() }
    }

    fn clear(&mut self) {
        for set in [&mut self.cold, &mut self.header, &mut self.likely, &mut self.freq, &mut self.remat, &mut self.nowrap] {
            set.clear();
        }
    }
}

/// `ProgramBuilder::scratch`: one function's at a time (`define` clears them).
struct Scratch {
    consts: IrVec<Option<(Type, u64)>>,
    marks: HintMarks,
}

/// Sets `i` in `set`; whether it was not set before.
fn mark_new(set: &mut IrVec<bool>, i: usize) -> bool {
    if set.len() <= i {
        set.resize(i + 1, false);
    }
    !core::mem::replace(&mut set[i], true)
}

fn marked(set: &[bool], i: usize) -> bool {
    set.get(i).copied().unwrap_or(false)
}

impl<'a> FunctionBuilder<'a> {
    pub fn func(&self) -> &Function {
        self.func
    }

    /// An edge in the program's root, for `terminate` (a branch the text loader keeps as written).
    pub fn edge(&self, target: Block, args: &[Value]) -> Edge {
        self.mint.edge(target, args)
    }

    /// Declare a host from inside a function body, once per program: a
    /// declaration with the same `name` (and the same signature and
    /// address, asserted) is returned instead of duplicated, so every
    /// call site of a builtin's shim in every function of the program
    /// shares one declaration and the program text carries it once.
    pub fn declare_host_once(&mut self, name: &str, params: &[Type], ret: Option<Type>, addr: usize, flags: HostFlags) -> HostFn {
        if let Some(i) = self.host_fns.iter().position(|h| h.name == name) {
            let h = &self.host_fns[i];
            assert!(h.params == params && h.ret == ret && h.addr == addr, "host {name} redeclared with another signature");
            return HostFn(i as u32);
        }
        let id = HostFn(self.host_fns.len() as u32);
        let decl = HostFnDecl { name: self.mint.str(name), params: self.mint.vec_from(params), ret, addr, flags };
        self.host_fns.push(decl);
        id
    }

    fn new_value(&mut self, ty: Type) -> Value {
        let v = Value(self.func.values.len() as u32);
        self.func.values.push(ty);
        v
    }

    pub fn value_type(&self, v: Value) -> Type {
        self.func.values[v.index()]
    }

    pub fn declare_stack_slot(&mut self, bytes: u32, align: u32) -> StackSlot {
        let id = StackSlot(self.func.stack_slots.len() as u32);
        self.func.stack_slots.push(StackSlotDecl { bytes, align });
        id
    }

    /// Create an empty block with the given parameter types. Its terminator is
    /// `Unreachable` until one is set.
    pub fn create_block(&mut self, param_types: &[Type]) -> Block {
        let mut params = self.mint.vec_with_capacity(param_types.len());
        for &t in param_types {
            params.push(self.new_value(t));
        }
        let id = Block(self.func.blocks.len() as u32);
        let insts = self.mint.vec();
        self.func.blocks.push(BasicBlock { params: IrSlice::from_vec(params), insts, term: Terminator::Unreachable });
        id
    }

    /// Number of blocks created so far (block ids are dense, in creation
    /// order: `Block(n)` for `n` below this is a block).
    pub fn num_blocks(&self) -> usize {
        self.func.blocks.len()
    }

    /// Whether the block has an explicit frequency hint or is marked cold.
    pub fn has_freq_hint(&self, b: Block) -> bool {
        marked(&self.marks.cold, b.index()) || marked(&self.marks.freq, b.index())
    }

    pub fn block_params(&self, b: Block) -> &[Value] {
        &self.func.blocks[b.index()].params
    }

    pub fn entry(&self) -> Block {
        self.func.entry
    }

    /// Parameters of the entry block, i.e. the function's arguments.
    pub fn params(&self) -> &[Value] {
        &self.func.blocks[self.func.entry.index()].params
    }

    pub fn switch_to(&mut self, b: Block) {
        self.current = Some(b);
    }

    pub fn current(&self) -> Block {
        self.current.expect("no current block")
    }

    /// The current block and its instruction count: a place `store_zero_at` fills later.
    pub fn cursor(&self) -> (Block, usize) {
        let b = self.current();
        (b, self.func.blocks[b.index()].insts.len())
    }

    /// An `I64` zero stored to `ptr + offset` at `at` (a `cursor`), before what the block gained since (`ptr` defined
    /// before `at`).
    pub fn store_zero_at(&mut self, at: (Block, usize), ptr: Value, offset: i32) {
        let z = self.new_value(Type::I64);
        if self.consts.len() <= z.index() {
            self.consts.resize(z.index() + 1, None);
        }
        self.consts[z.index()] = Some((Type::I64, 0));
        let store = Instruction { result: None, op: Op::Store { ptr, offset, value: z, unaligned: false } };
        if let Some(j) = self.jit.as_deref_mut() {
            j.inserted(at.0 .0, at.1 as u32, 2);
        }
        let insts = &mut self.func.blocks[at.0.index()].insts;
        insts.insert(at.1, store); // alloc: Program
        insts.insert(at.1, Instruction { result: Some(z), op: Op::Const { ty: Type::I64, bits: 0 } }); // alloc: Program
        self.pushed += 2;
    }

    fn push(&mut self, result: Option<Type>, op: Op) -> Option<Value> {
        let result = result.map(|t| self.new_value(t));
        let b = self.current();
        if let Some(j) = self.jit.as_deref_mut() {
            j.at(b.0, self.func.blocks[b.index()].insts.len() as u32);
        }
        let insts = &mut self.func.blocks[b.index()].insts;
        if insts.capacity() == 0 {
            // D3886: a block's first push reserves the function's mean block so far (`block_reserve`), where the empty
            // Vec grew 4, 8, 16, 32 .. (a copy each time): the grows under this push were ~5% of a lowering
            insts.reserve_exact(block_reserve(self.pushed, self.filled)); // alloc: Program
            self.filled += 1;
        }
        insts.push(Instruction { result, op });
        self.pushed += 1;
        result
    }

    fn push_val(&mut self, ty: Type, op: Op) -> Value {
        self.push(Some(ty), op).unwrap()
    }

    /// The instructions pushed into this function so far (a folded constant pushes none): `tag_lines` counts a span by it.
    pub fn pushed(&self) -> u32 {
        self.pushed
    }

    /// Attribute `lines` instructions to `tag` (a lowerer's plan node: the rows that run them), summed per tag. No effect on
    /// the IR; the backend's tier-0 break-even weighs them (gravity_backend `mapped_cached_keyed`'s sites).
    pub fn tag_lines(&mut self, tag: i32, lines: u32) {
        if lines == 0 {
            return;
        }
        match self.tagged.iter_mut().find(|t| t.0 == tag) {
            Some(t) => t.1 = t.1.saturating_add(lines),
            None => self.tagged.push((tag, lines)), // alloc: Program
        }
    }

    /// Whether this lowering records the JIT map's tags (`ProgramBuilder::jit_tags`): a caller builds a name only then.
    #[inline]
    pub fn jit_on(&self) -> bool {
        self.jit.is_some()
    }

    /// The instructions added from here on carry the tag named `name` (interned per program); returns the tag in effect
    /// before, for `restore_tag`. No effect on the IR; off (`jit_on` false) it returns 0 and `name` is not written.
    #[inline]
    pub fn set_tag(&mut self, name: core::fmt::Arguments<'_>) -> u32 {
        match self.jit.as_deref_mut() {
            Some(j) => j.set(name),
            None => 0,
        }
    }

    /// Puts back the tag `set_tag` returned.
    #[inline]
    pub fn restore_tag(&mut self, prev: u32) {
        if let Some(j) = self.jit.as_deref_mut() {
            j.cur = prev;
        }
    }

    /// The most recently pushed instruction of the current block.
    pub fn last_inst_mut(&mut self) -> Option<&mut Instruction> {
        let b = self.current?;
        self.func.blocks[b.index()].insts.last_mut()
    }

    // -- hints ---------------------------------------------------------------

    pub fn mark_cold(&mut self, b: Block) {
        if mark_new(&mut self.marks.cold, b.index()) {
            self.func.hints.cold_blocks.push(b);
        }
    }
    /// Whether `b` was marked cold.
    pub fn is_cold(&self, b: Block) -> bool {
        marked(&self.marks.cold, b.index())
    }
    pub fn set_freq(&mut self, b: Block, freq: f32) {
        if !mark_new(&mut self.marks.freq, b.index()) {
            self.func.hints.block_freq.retain(|(x, _)| *x != b);
        }
        self.func.hints.block_freq.push((b, freq));
    }
    pub fn mark_loop_header(&mut self, b: Block) {
        if mark_new(&mut self.marks.header, b.index()) {
            self.func.hints.loop_headers.push(b);
        }
    }
    /// Whether the function marks its loop headers: a backend then takes the marked blocks as its only loops.
    pub fn marks_loops(&self) -> bool {
        !self.func.hints.loop_headers.is_empty()
    }
    /// The block's `CondBr` likely takes its `then` edge (`true`) or `else`.
    pub fn set_likely(&mut self, b: Block, then: bool) {
        if !mark_new(&mut self.marks.likely, b.index()) {
            self.func.hints.likely.retain(|(x, _)| *x != b);
        }
        self.func.hints.likely.push((b, then));
    }
    pub fn mark_remat(&mut self, v: Value) {
        if mark_new(&mut self.marks.remat, v.index()) {
            self.func.hints.remat.push(v);
        }
    }
    pub fn mark_nowrap(&mut self, v: Value) {
        if mark_new(&mut self.marks.nowrap, v.index()) {
            self.func.hints.nowrap.push(v);
        }
    }

    pub fn terminate(&mut self, term: Terminator) {
        let b = self.current();
        if let Some(j) = self.jit.as_deref_mut() {
            j.at(b.0, self.func.blocks[b.index()].insts.len() as u32);
        }
        self.func.blocks[b.index()].term = term;
        self.current = None;
    }

    // -- constants -----------------------------------------------------------

    pub fn const_raw(&mut self, ty: Type, bits: u64) -> Value {
        let v = self.push_val(ty, Op::Const { ty, bits });
        if self.consts.len() <= v.index() {
            self.consts.resize(v.index() + 1, None);
        }
        self.consts[v.index()] = Some((ty, bits));
        v
    }
    /// The constant a value was built from, if any.
    pub fn const_of(&self, v: Value) -> Option<(Type, u64)> {
        self.consts.get(v.index()).copied().flatten()
    }
    pub fn iconst(&mut self, ty: Type, v: i64) -> Value {
        debug_assert!(ty.is_int() || ty == Type::Ptr);
        self.const_raw(ty, v as u64)
    }
    pub fn bconst(&mut self, v: bool) -> Value {
        self.const_raw(Type::Bool, v as u64)
    }
    pub fn f64const(&mut self, v: f64) -> Value {
        self.const_raw(Type::F64, v.to_bits())
    }
    pub fn f32const(&mut self, v: f32) -> Value {
        self.const_raw(Type::F32, v.to_bits() as u64)
    }

    // -- integer -------------------------------------------------------------

    pub fn int_bin(&mut self, op: IntBinOp, a: Value, b: Value) -> Value {
        let ty = self.value_type(a);
        // Boolean identities and absorption: no instruction at all.
        if ty == Type::Bool {
            let (ca, cb) = (self.const_of(a).map(|c| c.1 & 1), self.const_of(b).map(|c| c.1 & 1));
            match (op, ca, cb) {
                (IntBinOp::Or, _, Some(0)) | (IntBinOp::And, _, Some(1)) | (IntBinOp::Xor, _, Some(0)) => return a,
                (IntBinOp::Or, Some(0), _) | (IntBinOp::And, Some(1), _) | (IntBinOp::Xor, Some(0), _) => return b,
                (IntBinOp::Or, _, Some(1)) | (IntBinOp::Or, Some(1), _) => return self.bconst(true),
                (IntBinOp::And, _, Some(0)) | (IntBinOp::And, Some(0), _) => return self.bconst(false),
                _ => {}
            }
        } else if ty.is_int() {
            // zero identities and absorption (a raise mask's `or` over constant-0 masks stays constant)
            let (ca, cb) = (self.const_of(a).map(|c| c.1), self.const_of(b).map(|c| c.1));
            match (op, ca, cb) {
                (IntBinOp::Or | IntBinOp::Xor | IntBinOp::Add | IntBinOp::Sub, _, Some(0)) => return a,
                (IntBinOp::Or | IntBinOp::Xor | IntBinOp::Add, Some(0), _) => return b,
                (IntBinOp::And, _, Some(0)) | (IntBinOp::And, Some(0), _) => return self.iconst(ty, 0),
                _ => {}
            }
        }
        self.push_val(ty, Op::IntBin { op, a, b })
    }
    pub fn int_ovf(&mut self, op: IntOvfOp, a: Value, b: Value) -> Value {
        self.push_val(Type::Bool, Op::IntOvf { op, a, b })
    }
    pub fn int_un(&mut self, op: IntUnOp, a: Value) -> Value {
        let ty = self.value_type(a);
        self.push_val(ty, Op::IntUn { op, a })
    }
    pub fn icmp(&mut self, cond: IntCond, a: Value, b: Value) -> Value {
        self.push_val(Type::Bool, Op::Icmp { cond, a, b })
    }

    // -- float ---------------------------------------------------------------

    pub fn float_bin(&mut self, op: FloatBinOp, a: Value, b: Value) -> Value {
        let ty = self.value_type(a);
        self.push_val(ty, Op::FloatBin { op, a, b })
    }
    pub fn float_un(&mut self, op: FloatUnOp, a: Value) -> Value {
        let ty = self.value_type(a);
        self.push_val(ty, Op::FloatUn { op, a })
    }
    pub fn fcmp(&mut self, cond: FloatCond, a: Value, b: Value) -> Value {
        self.push_val(Type::Bool, Op::Fcmp { cond, a, b })
    }

    // -- misc ----------------------------------------------------------------

    pub fn conv(&mut self, op: ConvOp, to: Type, a: Value) -> Value {
        // A pointer/integer conversion of a value already of the target type
        // is the value (the verifier rejects `ptrtoint` of an `i64`).
        if matches!(op, ConvOp::PtrToInt | ConvOp::IntToPtr) && self.value_type(a) == to {
            return a;
        }
        // Integer conversions of constants fold to a constant of the target.
        if let Some((_, bits)) = self.const_of(a) {
            if matches!(op, ConvOp::Sext | ConvOp::Zext | ConvOp::Trunc | ConvOp::PtrToInt | ConvOp::IntToPtr) && !to.is_float() {
                let from = self.value_type(a);
                let v = match op {
                    ConvOp::Zext => match from.bytes() { 1 => bits & 0xff, 2 => bits & 0xffff, 4 => bits & 0xffff_ffff, _ => bits },
                    _ => bits,
                };
                return self.const_raw(to, v);
            }
            // A bit pattern reinterpreted is still a constant (so float
            // constants built from Datum bits are visible as constants).
            if op == ConvOp::Bitcast {
                let v = match to.bytes() { 4 => bits & 0xffff_ffff, _ => bits };
                return self.const_raw(to, v);
            }
        }
        self.push_val(to, Op::Conv { op, to, a })
    }
    pub fn select(&mut self, cond: Value, a: Value, b: Value) -> Value {
        // A constant condition picks its operand at build time.
        if let Some((_, bits)) = self.const_of(cond) {
            return if bits & 1 != 0 { a } else { b };
        }
        if a == b {
            return a;
        }
        let ty = self.value_type(a);
        self.push_val(ty, Op::Select { cond, a, b })
    }
    pub fn load(&mut self, ty: Type, ptr: Value, offset: i32) -> Value {
        self.push_val(ty, Op::Load { ty, ptr, offset, unaligned: false })
    }
    pub fn load_unaligned(&mut self, ty: Type, ptr: Value, offset: i32) -> Value {
        self.push_val(ty, Op::Load { ty, ptr, offset, unaligned: true })
    }
    /// Zero-extending load of a narrow integer to `I64`.
    pub fn load_zext(&mut self, ty: Type, ptr: Value, offset: i32) -> Value {
        debug_assert!(matches!(ty, Type::I8 | Type::I16 | Type::I32));
        self.push_val(Type::I64, Op::LoadZext { ty, ptr, offset, unaligned: false })
    }
    /// `load_zext` of a narrow integer at any alignment (text bytes).
    pub fn load_zext_unaligned(&mut self, ty: Type, ptr: Value, offset: i32) -> Value {
        debug_assert!(matches!(ty, Type::I8 | Type::I16 | Type::I32));
        self.push_val(Type::I64, Op::LoadZext { ty, ptr, offset, unaligned: true })
    }
    pub fn store(&mut self, ptr: Value, offset: i32, value: Value) {
        self.push(None, Op::Store { ptr, offset, value, unaligned: false });
    }
    pub fn store_unaligned(&mut self, ptr: Value, offset: i32, value: Value) {
        self.push(None, Op::Store { ptr, offset, value, unaligned: true });
    }
    /// Prefetch hint for `ptr + offset` (no result).
    pub fn prefetch(&mut self, ptr: Value, offset: i32) {
        self.push(None, Op::Prefetch { ptr, offset });
    }
    /// CRC-32C of `data` folded into `acc` (both `I64`; `I64` result).
    pub fn crc32c(&mut self, acc: Value, data: Value) -> Value {
        self.push_val(Type::I64, Op::Crc32c { acc, data })
    }
    /// The high word of `ahi:alo + bhi:blo` (all `I64`).
    pub fn add_hi(&mut self, alo: Value, blo: Value, ahi: Value, bhi: Value) -> Value {
        self.push_val(Type::I64, Op::AddHi { alo, blo, ahi, bhi })
    }
    pub fn stack_addr(&mut self, slot: StackSlot) -> Value {
        self.push_val(Type::Ptr, Op::StackAddr { slot })
    }
    pub fn ptr_add(&mut self, base: Value, index: Value, scale: u8, offset: i32) -> Value {
        self.push_val(Type::Ptr, Op::PtrAdd { base, index, scale, offset })
    }

    /// Call an internal function. `ret` is its return type (`None` for void);
    /// the builder cannot see other functions, so the caller supplies it.
    pub fn call(&mut self, callee: Func, args: &[Value], ret: Option<Type>) -> Option<Value> {
        let args = self.mint.slice_from(args);
        self.push(ret, Op::Call { callee, args })
    }
    pub fn call_host(&mut self, callee: HostFn, args: &[Value], ret: Option<Type>) -> Option<Value> {
        let args = self.mint.slice_from(args);
        self.push(ret, Op::CallHost { callee, args })
    }

    // -- vectors (docs/design/gravity-simple/simd.md) ------------------------

    pub fn vload(&mut self, ty: Type, ptr: Value, offset: i32) -> Value {
        debug_assert!(ty.is_vector());
        self.push_val(ty, Op::VecLoad { ty, ptr, offset, unaligned: false })
    }
    pub fn vload_unaligned(&mut self, ty: Type, ptr: Value, offset: i32) -> Value {
        debug_assert!(ty.is_vector());
        self.push_val(ty, Op::VecLoad { ty, ptr, offset, unaligned: true })
    }
    pub fn vstore(&mut self, ptr: Value, offset: i32, value: Value) {
        self.push(None, Op::VecStore { ptr, offset, value, unaligned: false });
    }
    pub fn vstore_unaligned(&mut self, ptr: Value, offset: i32, value: Value) {
        self.push(None, Op::VecStore { ptr, offset, value, unaligned: true });
    }
    pub fn vbin(&mut self, op: VecBinOp, a: Value, b: Value) -> Value {
        let ty = self.value_type(a);
        self.push_val(ty, Op::VecBin { op, a, b })
    }
    pub fn vshift(&mut self, op: VecShiftOp, a: Value, count: u8) -> Value {
        let ty = self.value_type(a);
        self.push_val(ty, Op::VecShift { op, a, count })
    }
    pub fn vcmp(&mut self, cond: IntCond, a: Value, b: Value) -> Value {
        let ty = self.value_type(a);
        self.push_val(ty, Op::VecCmp { cond, a, b })
    }
    pub fn vselect(&mut self, mask: Value, a: Value, b: Value) -> Value {
        let ty = self.value_type(a);
        self.push_val(ty, Op::VecSelect { mask, a, b })
    }
    /// See `Op::VecReduceAdd` for the result type per vector type.
    pub fn vreduce_add(&mut self, a: Value) -> Value {
        let r = match self.value_type(a) {
            Type::V2F64 => Type::F64,
            Type::V4F32 => Type::F32,
            _ => Type::I64,
        };
        self.push_val(r, Op::VecReduceAdd { a })
    }
    pub fn vmovmask(&mut self, a: Value) -> Value {
        self.push_val(Type::I64, Op::VecMovmask { a })
    }
    pub fn vsplat(&mut self, ty: Type, a: Value) -> Value {
        debug_assert!(ty.is_vector());
        self.push_val(ty, Op::VecSplat { ty, a })
    }
    pub fn vextract(&mut self, a: Value, lane: u8) -> Value {
        let ty = self.value_type(a).lane();
        self.push_val(ty, Op::VecExtract { a, lane })
    }
    pub fn vinsert(&mut self, a: Value, lane: u8, value: Value) -> Value {
        let ty = self.value_type(a);
        self.push_val(ty, Op::VecInsert { a, lane, value })
    }
    pub fn vwiden(&mut self, a: Value, signed: bool, high: bool) -> Value {
        let ty = self.value_type(a).widened().expect("vwiden of a widenable vector");
        self.push_val(ty, Op::VecWiden { a, signed, high })
    }
    pub fn vnarrow(&mut self, a: Value, b: Value) -> Value {
        let ty = self.value_type(a).narrowed().expect("vnarrow of a narrowable vector");
        self.push_val(ty, Op::VecNarrow { a, b })
    }
    /// Reinterpret the 128 bits of `a` as vector type `to` (free in the
    /// backends: the register does not move).
    pub fn vbitcast(&mut self, to: Type, a: Value) -> Value {
        if self.value_type(a) == to {
            return a;
        }
        self.push_val(to, Op::Conv { op: ConvOp::Bitcast, to, a })
    }

    // -- terminators ---------------------------------------------------------

    pub fn br(&mut self, target: Block, args: &[Value]) {
        let e = self.mint.edge(target, args);
        self.terminate(Terminator::Br(e));
    }
    pub fn cond_br(&mut self, cond: Value, then: (Block, &[Value]), els: (Block, &[Value])) {
        // A constant condition is an unconditional branch.
        if let Some((_, bits)) = self.const_of(cond) {
            let (b, args) = if bits & 1 != 0 { then } else { els };
            return self.br(b, args);
        }
        let (then_edge, else_edge) = (self.mint.edge(then.0, then.1), self.mint.edge(els.0, els.1));
        self.terminate(Terminator::CondBr { cond, then_edge, else_edge });
    }
    /// A multi-way branch on `value`: each case `(bits, target, args)`, then `default`.
    pub fn switch<A: AsRef<[Value]>>(&mut self, value: Value, cases: impl IntoIterator<Item = (u64, Block, A)>, default: (Block, &[Value])) {
        let mut cs = self.mint.vec();
        for (v, t, args) in cases {
            cs.push(SwitchCase { value: v, edge: self.mint.edge(t, args.as_ref()) });
        }
        let default = self.mint.edge(default.0, default.1);
        self.terminate(Terminator::Switch { value, cases: IrSlice::from_vec(cs), default });
    }
    pub fn ret(&mut self, values: &[Value]) {
        let vs = self.mint.slice_from(values);
        self.terminate(Terminator::Return(vs));
    }
    pub fn unreachable(&mut self) {
        self.terminate(Terminator::Unreachable);
    }
}

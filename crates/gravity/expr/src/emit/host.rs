//! The call cells (expr-compiler.md §2.4): a tier B / C site calls through `Cell` — the emitted
//! code stores the arguments, calls `shim` / `fmgr` and reads `out`; the status is the return.

use gravity_abi::{word, PtrWord, Word};
use core::mem::{offset_of, size_of};

use gravity_fn::{Arg, ArgKind, Datum, ErrSlot, Family, FmgrCall, Region, Shim};

use crate::nodes::array::{ElemFacts, Walk};
use crate::nodes::func::{ArgConv, Call, How, MAX_ARGS};
use crate::eval::{Cols, Cx, Row, Source};
use crate::nodes::{ExprNode, HostedProgram, HostedState, NodeId, Prog, Program, State};
use crate::views::{image_view, payload_view};
use crate::{Compiled, HostedCompiled};

/// A site's cell: this header, then `width` argument words (`CELL_ARGS`), then `width` null bytes (`cell_nulls`), the
/// next cell `cell_size(width)` bytes on. D3620: `width` is the program's widest site (`cell_width`), not `MAX_ARGS`:
/// every cell was 952 bytes, ~6.7 KB a hosted owner, per nesting level of a recursive SQL function.
#[repr(C)]
pub struct Cell {
    pub call: Word<*const Call>,
    pub convs: Word<*const ArgConv>,
    pub fmgr: Word<*mut FmgrCall>,
    pub err: ErrSlot,
    pub out: i64,
    /// A synthetic site's owner-side fact the IR loads as an argument: a `RowExpr` / `FieldSelect` site's descriptor
    /// pointer (`State::descs`, live for the program's life; never in the content key). A walked ScalarArrayOp's operator
    /// site (a function's, never synthetic): the walk word the IR stores before each walk (`walk_word`), which `shim` takes
    /// as a walk (and clears). 0 for every other site.
    pub aux: i64,
    /// The argument words (and null bytes) after this header: the program's `cell_width`. A host finds the null bytes by
    /// it (`Cell::nulls`); the emitted code has it as a constant.
    pub width: u64,
}

pub const CELL_OUT: i32 = offset_of!(Cell, out) as i32;
pub const CELL_AUX: i32 = offset_of!(Cell, aux) as i32;
pub const CELL_ERR: i32 = offset_of!(Cell, err) as i32;
/// The first argument word: right after the header.
pub const CELL_ARGS: i32 = size_of::<Cell>() as i32;

/// The argument words (and null bytes) every cell of `prog` holds: its widest site's. A wide row / array's continuation
/// sites (`nodes::row::continuation_sites`) are `MAX_ARGS` wide, so a program with one keeps `MAX_ARGS`.
pub fn cell_width(prog: &Program<'_>) -> usize {
    sites_width(&prog.sites)
}

/// `cell_width` over a program's sites wherever its lists are (a template's: `EmitState::of`).
fn sites_width(sites: &[Call]) -> usize {
    sites.iter().map(|c| c.nargs as usize).max().unwrap_or(0)
}

/// The null bytes of a cell `width` wide: after its argument words.
pub const fn cell_nulls(width: usize) -> i32 {
    CELL_ARGS + (width * 8) as i32
}

/// The bytes from a cell `width` wide to the next: the header, the words and the null bytes, to a word.
pub const fn cell_size(width: usize) -> usize {
    (CELL_ARGS as usize + width * 9).next_multiple_of(8)
}

impl Cell {
    /// A site's header (`width` set by `EmitState::push`).
    pub fn head(call: *const Call, convs: *const ArgConv, fmgr: *mut FmgrCall, aux: i64) -> Cell {
        Cell { call: word(call), convs: word(convs), fmgr: word(fmgr), err: ErrSlot::NONE, out: 0, aux, width: 0 }
    }

    /// The cell's argument words.
    /// # Safety — `c` is a cell of a live `EmitState` (its words follow the header); nothing else holds them.
    #[inline(always)]
    pub unsafe fn args<'a>(c: *mut Cell) -> &'a mut [i64] {
        core::slice::from_raw_parts_mut(c.cast::<u8>().add(CELL_ARGS as usize).cast::<i64>(), (*c).width as usize)
    }

    /// The cell's null bytes.
    /// # Safety — as `args`.
    #[inline(always)]
    pub unsafe fn nulls<'a>(c: *mut Cell) -> &'a mut [u8] {
        let w = (*c).width as usize;
        core::slice::from_raw_parts_mut(c.cast::<u8>().add(cell_nulls(w) as usize), w)
    }
}

/// The `Family::ByVal` shims that return `NULL` (`null_ret` in `shims/byval.rs`; `expr/tests/emit.rs` pins the list).
pub const NULLABLE_BYVAL: &[&str] = &["timestamp_time", "timestamptz_time"];

/// The shim a site calls directly (lowerv2 M2 L5-SHIM (a)): a strict `Family::ByVal` shim with no collation, no
/// resolved types, every argument a word and a result that is never NULL. The emitted code calls its typed entry
/// with the site's error slot and the words in registers and reads `Ret::val` from the first return register; an
/// error is the slot's non-null pointer after the call (`Ret::status`, the second register, is not read). That call
/// is emitted on 64-bit targets only (D3129: `emit::scalar`'s `direct`); on wasm32 the same site takes the cell road.
/// Which sites qualify (this function, `late_roots`) is the same on every target.
pub fn direct(p: &Program<'_>, call: &Call) -> Option<&'static Shim> {
    let How::Shim { shim, convs, byref: false } = call.how else { return None };
    if !call.strict || shim.family != Family::ByVal || shim.coll || !shim.tyargs.is_empty() || NULLABLE_BYVAL.contains(&shim.name) {
        return None;
    }
    let n = call.nargs as usize;
    p.convs[convs as usize..convs as usize + n].iter().all(|c| c.kind == ArgKind::V).then_some(shim)
}

/// D3108: a strict shim with a by-value result and no region whose every by-reference argument is a leaf: a literal, an
/// immediate or a parameter, whose image is the bind image's or the parameter's and so holds still with the parameters
/// (never a call's region image, which the next row's reset frees). Its collation is the site's constant. `late_roots`
/// computes such a call once at its hoist scope as it does a direct shim.
pub fn leaf_images(p: &Program<'_>, call: &Call) -> bool {
    let How::Shim { shim, convs, byref: false } = call.how else { return false };
    if !call.strict || shim.region || !shim.tyargs.is_empty() || NULLABLE_BYVAL.contains(&shim.name) || !matches!(shim.family, Family::ByVal | Family::ByRefVal | Family::ViewVal) {
        return false;
    }
    let n = call.nargs as usize;
    if call.args.len as usize != n {
        return false;
    }
    fn leaf(p: &Program<'_>, k: NodeId) -> bool {
        crate::stack_check();
        match &p.nodes[k.0 as usize] {
            // D2834: a deferred parameter is a fetch where it is evaluated, never an image held still with the words
            ExprNode::Param(q) => !crate::nodes::param::is_deferred(q),
            ExprNode::Const(_) | ExprNode::Imm(_) => true,
            ExprNode::Relabel(r) => leaf(p, r.arg),
            _ => false,
        }
    }
    let args = &p.kids[call.args.start as usize..call.args.start as usize + n];
    p.convs[convs as usize..convs as usize + n].iter().zip(args).all(|(c, &k)| c.kind == ArgKind::V || leaf(p, k))
}

pub const STATUS_OK: i64 = 0;
pub const STATUS_NULL: i64 = 1;
pub const STATUS_ERROR: i64 = -1;

/// One cell per site of a program, `cell_size(width)` bytes apart in one buffer; the program (its sites, convs and fmgr
/// frames) outlives it.
pub struct EmitState<'m> {
    /// The cells (words: every header field and argument word is 8-aligned).
    /// In the context it was made in (`with_cells`): the planning context while the lowering builds it, then the owner's
    /// product root (`into_in`).
    buf: mcx::PgVec<'m, u64>,
    n: usize,
    width: usize,
    /// The interpreter islands' host-side facts (`Cell::aux` of a `How::Island` site; boxed, the address holds).
    pub islands: Vec<Box<IslandHost>>,
}

/// An island's owner-side fact: the program, state and binding (raw, live for the program's life), the root, and
/// per argument the source / column it lays into the interpreter's row.
pub struct IslandHost {
    pub prog: *const (),
    /// The program's state, which `eval` reads with `prog`: a compiled program's `State` (`EmitState::new`,
    /// `eval_heap`) or a hosted owner's `HostedState` in its product root (`rebind_all`, `eval_hosted`).
    pub state: *mut (),
    pub eval: Interp,
    pub params: *const (),
    pub callback_arg: *mut (),
    pub root: NodeId,
    pub slots: Vec<(Source, i16)>,
    pub values: [Vec<Datum>; 3],
    pub nulls: [Vec<bool>; 3],
    /// The island's parameters `(executor, paramid, type)`, its arguments after the columns (nodes/mod.rs `island`), and the
    /// lists the interpreter reads them from (extern ones by id, V4R-454 executor ones by id): this call's words, bound per
    /// execution, never the compile's binding.
    pub prms: Vec<(bool, i32, types_core::Oid)>,
    pub pext: Vec<types_portal::params::ParamExternData>,
    pub pexec: Vec<types_portal::params::ParamExecData>,
    /// The island's aggregate and SubPlan cells by number (its arguments between the columns and the parameters) and the row's
    /// `aggs` the interpreter's `Aggref` reads them from: each call's finished cells, never a partial state.
    pub aggnos: Vec<i32>,
    pub aggs: Vec<gravity_fn::NullableDatum>,
    /// D901: a `Once` island's kept value (nodes/mod.rs `Once`); `None` for an island evaluated per row.
    pub once: Option<OnceCache>,
}

/// D901 (M6-AGG, main 07:39Z): a built-in IMMUTABLE island over extern parameters alone keeps the value of the last call
/// that returned one, with the argument values it came from; a call whose arguments are the same values returns it
/// without evaluating. The first call that reaches the island computes it (lazily: no row, a branch not taken, a
/// conjunct that failed first reach nothing, and an error raises where PG's per-row evaluation would); an error is never
/// kept. A varlena argument or result keeps only a plain image (a 4-byte uncompressed or a short header): an external
/// or compressed one is never kept, the call evaluates. A kept varlena result is copied into the call's region on
/// every hit, so the value handed out has an evaluated result's lifetime (the kept image itself is never handed out).
pub struct OnceCache {
    varlena: u32,
    ret_byval: bool,
    /// Per argument its null byte, then its word (by value, 8 bytes) or its image (a varlena: 4-byte length, the bytes).
    key: Vec<u8>,
    /// The kept value: `(null, word)`; a varlena result's word is unused, its image is `img[..len]`.
    kept: Option<(bool, i64)>,
    img: Vec<u64>,
    len: usize,
    /// The calls that evaluated (a miss, an error, or a value not kept): the tests' count.
    evals: u64,
}

impl OnceCache {
    fn new(o: crate::nodes::Once) -> OnceCache {
        OnceCache { varlena: o.varlena, ret_byval: o.ret_byval, key: Vec::new(), kept: None, img: Vec::new(), len: 0, evals: 0 } // alloc: Program
    }

    /// The calls that evaluated the island (every other call returned the kept value).
    pub fn evals(&self) -> u64 {
        self.evals
    }

    /// A plain varlena at `p` (not external, not compressed): its length.
    /// # Safety — `p` is a live varlena image.
    unsafe fn plain(p: *const u8) -> Option<usize> {
        use types_tuple::varatt;
        (varatt::varatt_is_4b_u(p) || (varatt::varatt_is_1b(p) && !varatt::varatt_is_1b_e(p))).then(|| varatt::varsize_any(p))
    }

    /// The kept value when the cell's `n` arguments (`args`, `nulls`) are the values it was computed from: `(true, _)` null, `(false, w)` the
    /// word (a varlena result: a copy of the kept image in `region`; `None`, the call evaluates, when the region is full).
    /// # Safety — `c`'s varlena arguments are live images; `region` a live region.
    unsafe fn hit(&self, args: &[i64], nulls: &[u8], n: usize, region: &mut Region) -> Option<(bool, i64)> {
        let kept = self.hit_of(args, nulls, n)?;
        if kept.0 || self.ret_byval {
            return Some(kept);
        }
        let p = region.reserve(self.len)?;
        core::ptr::copy_nonoverlapping(self.img.as_ptr() as *const u8, p, self.len);
        Some((false, p as usize as i64))
    }

    /// # Safety — as `hit`.
    unsafe fn hit_of(&self, args: &[i64], nulls: &[u8], n: usize) -> Option<(bool, i64)> {
        let kept = self.kept?;
        let (key, mut at) = (&self.key[..], 0usize);
        for i in 0..n {
            let null = nulls[i] != 0;
            if key.get(at) != Some(&(null as u8)) {
                return None;
            }
            at += 1;
            if null {
                continue;
            }
            if self.varlena >> i & 1 == 0 {
                if key.get(at..at + 8) != Some(&args[i].to_le_bytes()[..]) {
                    return None;
                }
                at += 8;
                continue;
            }
            let p = args[i] as usize as *const u8;
            let len = Self::plain(p)?;
            let Some(l) = key.get(at..at + 4) else { return None };
            if u32::from_le_bytes([l[0], l[1], l[2], l[3]]) as usize != len || key.get(at + 4..at + 4 + len) != Some(core::slice::from_raw_parts(p, len)) {
                return None;
            }
            at += 4 + len;
        }
        (at == key.len()).then_some(kept)
    }

    /// Keep the value `(null, word)` computed from the cell's `n` arguments (a varlena result: a copy of its image); nothing
    /// is kept where an argument or the result is not a plain image.
    /// # Safety — `c`'s varlena arguments are live images; a non-null varlena result's word is a live image.
    unsafe fn keep(&mut self, args: &[i64], nulls: &[u8], n: usize, null: bool, word: i64) {
        self.kept = None;
        self.key.clear();
        for i in 0..n {
            let an = nulls[i] != 0;
            self.key.push(an as u8); // alloc: Program
            if an {
                continue;
            }
            if self.varlena >> i & 1 == 0 {
                self.key.extend_from_slice(&args[i].to_le_bytes()); // alloc: Program
                continue;
            }
            let p = args[i] as usize as *const u8;
            let Some(len) = Self::plain(p) else { return };
            self.key.extend_from_slice(&(len as u32).to_le_bytes()); // alloc: Program
            self.key.extend_from_slice(core::slice::from_raw_parts(p, len)); // alloc: Program
        }
        if null || self.ret_byval {
            self.kept = Some((null, word));
            return;
        }
        let p = word as usize as *const u8;
        let Some(len) = Self::plain(p) else { return };
        self.img.clear();
        self.img.resize(len.div_ceil(8), 0); // alloc: Program
        core::ptr::copy_nonoverlapping(p, self.img.as_mut_ptr() as *mut u8, len);
        self.len = len;
        self.kept = Some((false, 0));
    }
}

/// The interpreter over an island's program and state (`IslandHost::eval`): `eval_heap` or `eval_hosted`,
/// each the one tier-0 interpreter over the lists' views.
pub type Interp = unsafe fn(*const (), *mut (), NodeId, &mut Cx<'_, '_>) -> types_error::PgResult<gravity_fn::NullableDatum>;

/// `Interp` over a compiled program (`EmitState::new`).
/// # Safety — `prog` and `state` are a live `Program` and its `State`, neither reached otherwise meanwhile.
unsafe fn eval_heap(prog: *const (), state: *mut (), root: NodeId, cx: &mut Cx<'_, '_>) -> types_error::PgResult<gravity_fn::NullableDatum> {
    crate::eval::eval(&(*(prog as *const Program<'static>)).view(), &mut (*(state as *mut State)).view(), root, cx)
}

/// `Interp` over a hosted owner's program (`EmitState::rebind_all`).
/// # Safety — `prog` and `state` are a live `HostedProgram` and its `HostedState`, neither reached otherwise meanwhile.
unsafe fn eval_hosted(prog: *const (), state: *mut (), root: NodeId, cx: &mut Cx<'_, '_>) -> types_error::PgResult<gravity_fn::NullableDatum> {
    crate::eval::eval(&(*(prog as *const HostedProgram)).view(), &mut (*(state as *mut HostedState)).view(), root, cx)
}

/// A site's argument forms and function-manager frame (`Cell::head`'s `convs` and `fmgr`) in the lists at `convs` and
/// `calls` (a program's `convs`, its state's `calls`).
fn site_ptrs(convs: *const ArgConv, calls: *mut FmgrCall, call: &Call) -> (*const ArgConv, *mut FmgrCall) {
    match call.how {
        How::Shim { convs: at, .. } => (convs.wrapping_add(at as usize), core::ptr::null_mut()),
        How::Fmgr(ix) => (core::ptr::null(), calls.wrapping_add(ix as usize)),
        How::Island { .. } => (core::ptr::null(), core::ptr::null_mut()),
    }
}

impl<'m> EmitState<'m> {
    pub fn new(c: &mut Compiled<'_>, m: mcx::Mcx<'m>) -> EmitState<'m> {
        let mut cells = Self::of(&c.prog, &mut c.state, m);
        let mut islands: Vec<Box<IslandHost>> = Vec::new(); // alloc: Program
        for (i, call) in c.prog.sites.iter().enumerate() {
            let How::Island { root } = call.how else { continue };
            let mut slots = Vec::with_capacity(call.nargs as usize); // alloc: Program
            let mut prms = Vec::new(); // alloc: Program
            let mut aggnos: Vec<i32> = Vec::new(); // alloc: Program
            let mut natts = [0usize; 3];
            for k in 0..call.nargs as usize {
                match &c.prog.nodes[c.prog.kids[call.args.start as usize + k].0 as usize] {
                    ExprNode::Var(v) => {
                        slots.push((v.src, v.attno));
                        natts[v.src as usize] = natts[v.src as usize].max(v.attno as usize);
                    }
                    ExprNode::Aggref(g) => aggnos.push(g.aggno), // alloc: Program
                    // json-0033: a SubPlan's value is a cell of the same row `aggs` (nodes/srf.rs `eval_subplan`)
                    ExprNode::SubPlan(q) => aggnos.push(q.cell), // alloc: Program
                    ExprNode::Param(q) => prms.push((q.kind == types_nodes::primnodes::ParamKind::PARAM_EXEC, q.id, q.typ)), // alloc: Program
                    _ => unreachable!("an island's arguments are column Vars, aggregate / SubPlan cells, then Params"),
                }
            }
            let unset = types_portal::params::ParamExternData { value: Datum::null(), isnull: true, pflags: 0, ptype: 0 };
            let pext = vec![unset; prms.iter().filter(|p| !p.0).map(|&(_, id, _)| id.max(0) as usize).max().unwrap_or(0)]; // alloc: Program
            let pexec = vec![types_portal::params::ParamExecData::EMPTY; prms.iter().filter(|p| p.0).map(|&(_, id, _)| id.max(0) as usize + 1).max().unwrap_or(0)]; // alloc: Program
            let values = [vec![Datum::null(); natts[0]], vec![Datum::null(); natts[1]], vec![Datum::null(); natts[2]]]; // alloc: Program
            let nulls = [vec![true; natts[0]], vec![true; natts[1]], vec![true; natts[2]]]; // alloc: Program
            let aggs = vec![gravity_fn::NullableDatum { value: Datum::null(), isnull: true }; aggnos.iter().map(|&n| n.max(0) as usize + 1).max().unwrap_or(0)]; // alloc: Program
            let once = c.prog.islands.iter().find(|isl| isl.site.0 as usize == i).and_then(|isl| isl.once).map(OnceCache::new);
            let h = Box::new(IslandHost { prog: &c.prog as *const Program<'_> as *const (), state: &mut c.state as *mut State as *mut (), eval: eval_heap, params: &c.params as *const _ as *const (), callback_arg: c.callback_arg, root, slots, values, nulls, prms, pext, pexec, aggnos, aggs, once }); // alloc: Program
            cells.cell_mut(i).aux = &*h as *const IslandHost as i64;
            islands.push(h);
        }
        cells.islands = islands;
        cells
    }

    /// `new`'s cells for a program with no interpreter island: its sites over `prog`'s argument forms and `state`'s calls
    /// and descriptors (D3438: a program a process-wide template shares, its lists in the template's root, beside this
    /// backend's `state`).
    pub fn of(prog: &Program<'_>, state: &mut State, m: mcx::Mcx<'m>) -> EmitState<'m> {
        Self::of_view(&prog.view(), state, m)
    }

    /// `of` over a program whose lists moved (`Program::into_hosted`: a template's, lowerv4 `hir::rebuild`).
    pub fn of_hosted(prog: &HostedProgram, state: &mut State, m: mcx::Mcx<'m>) -> EmitState<'m> {
        Self::of_view(&prog.view(), state, m)
    }

    fn of_view(prog: &Prog<'_, '_>, state: &mut State, m: mcx::Mcx<'m>) -> EmitState<'m> {
        let mut cells = EmitState::with_cells(prog.sites.len(), sites_width(prog.sites), m);
        let (convs, calls) = (prog.convs.as_ptr(), state.calls.as_mut_ptr());
        for call in prog.sites.iter() {
            let (convs, fmgr) = site_ptrs(convs, calls, call);
            cells.push(Cell::head(call, convs, fmgr, 0));
        }
        cells.auxes(prog, &state.descs);
        cells
    }

    /// The synthetic sites' `aux`: their descriptors (`descs`, the program's state's) and names in `prog`.
    fn auxes(&mut self, prog: &Prog<'_, '_>, descs: &[crate::nodes::tuple::Desc]) {
        for n in prog.nodes.iter() {
            let (site, desc) = match n {
                // the conversion reads its two descriptors by index from the program's table
                ExprNode::ConvertRowtype(cv) => {
                    self.cell_mut(cv.site.0 as usize).aux = descs.as_ptr() as i64;
                    continue;
                }
                // D2019: the domain raise's names (`cast::DOMAIN_NAMES`, then its CHECKs'), live for the program's life
                ExprNode::CoerceToDomain(d) => {
                    self.cell_mut(d.site.0 as usize).aux = prog.names[(d.names - crate::nodes::cast::DOMAIN_NAMES) as usize..].as_ptr() as i64;
                    continue;
                }
                ExprNode::Row(r) => (r.site, r.desc),
                ExprNode::FieldSelect(f) if f.desc != crate::nodes::field::NO_DESC => (f.site, f.desc),
                // the store's site reads its named rowtype's descriptor the same way (emit/store.rs)
                ExprNode::FieldStore(f) => (f.site, f.desc),
                _ => continue,
            };
            self.cell_mut(site.0 as usize).aux = descs[desc as usize].desc.as_ref().expect("a compiled node's descriptor") as *const _ as i64;
        }
    }

    /// Room for `n` cells `width` wide, all zero (no cell yet: `push` writes their headers in site order).
    pub fn with_cells(n: usize, width: usize, m: mcx::Mcx<'m>) -> EmitState<'m> {
        EmitState { buf: mcx::vec_elem_in(m, 0u64, n * cell_size(width) / 8), n: 0, width, islands: Vec::new() } // alloc: Program
    }

    /// These cells in `m` (the owner's product root): the words copied, the old buffer freed. Nothing points into the
    /// cells (a lane's `cells` word is taken from `ptr` after), and an island's `aux` is its boxed `IslandHost`.
    pub fn into_in<'b>(mut self, m: mcx::Mcx<'b>) -> EmitState<'b> {
        let mut buf = mcx::reserved_in(self.buf.len(), m);
        mcx::GrowIn::extend_from_slice_in(&mut buf, &self.buf);
        let islands = core::mem::take(&mut self.islands);
        let (n, width) = (self.n, self.width);
        // the old cells' headers move with the words: the source must not drop them
        self.n = 0;
        EmitState { buf, n, width, islands }
    }

    /// The next site's cell: `head` with this state's width, its words and null bytes zero.
    pub fn push(&mut self, mut head: Cell) {
        let at = self.n * cell_size(self.width) / 8;
        assert!(at + cell_size(self.width) / 8 <= self.buf.len(), "EmitState::push past with_cells' room");
        head.width = self.width as u64;
        // SAFETY: `at` is cell `n`'s first word, inside `buf` (checked above) and 8-aligned; the slot holds no header yet
        // (the zero words are not a live `ErrSlot`), so nothing is dropped by the write.
        unsafe { self.buf.as_mut_ptr().add(at).cast::<Cell>().write(head) };
        self.n += 1;
    }

    /// The cells.
    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// The argument words (and null bytes) each cell holds (`cell_width`).
    pub fn width(&self) -> usize {
        self.width
    }

    /// Cell `i`'s header.
    pub fn cell(&self, i: usize) -> &Cell {
        assert!(i < self.n, "EmitState::cell {i} of {}", self.n);
        // SAFETY: cell `i` < `n` was written by `push`; the borrow of `self` keeps the buffer.
        unsafe { &*self.buf.as_ptr().add(i * cell_size(self.width) / 8).cast::<Cell>() }
    }

    /// Cell `i`'s header, mutable.
    pub fn cell_mut(&mut self, i: usize) -> &mut Cell {
        assert!(i < self.n, "EmitState::cell_mut {i} of {}", self.n);
        // SAFETY: as `cell`.
        unsafe { &mut *self.buf.as_mut_ptr().add(i * cell_size(self.width) / 8).cast::<Cell>() }
    }

    /// The headers, in site order.
    pub fn heads(&self) -> impl Iterator<Item = &Cell> {
        (0..self.n).map(move |i| self.cell(i))
    }

    /// The headers, in site order, mutable.
    pub fn heads_mut(&mut self) -> impl Iterator<Item = &mut Cell> {
        let (base, stride, n) = (self.buf.as_mut_ptr(), cell_size(self.width) / 8, self.n);
        // SAFETY: cells `0..n` hold headers `push` wrote, each its own `stride` words (disjoint); the iterator holds the
        // state's mutable borrow, so nothing else reaches them meanwhile.
        (0..n).map(move |i| unsafe { &mut *base.add(i * stride).cast::<Cell>() })
    }

    /// A lane's cells (lowerv4 `arm_lane`): each site's call, argument forms and `aux` as here, its function-manager frame a
    /// twin of this one's (`FmgrCall::twin_into`, kept in `calls`; `pool`: a pool thread's), a fresh error slot, result
    /// and words. No islands: a lane past 0 never reaches one.
    pub fn twin<'b>(&self, calls: &mut Vec<Box<FmgrCall>>, pool: bool, m: mcx::Mcx<'b>) -> EmitState<'b> {
        let mut t = EmitState::with_cells(self.n, self.width, m);
        for c in self.heads() {
            // SAFETY: `c.fmgr` is null or this state's live frame (the program's `state.calls`, which outlives the cells).
            let fmgr = unsafe { FmgrCall::twin_into(c.fmgr.ptr(), calls, pool) };
            t.push(Cell::head(c.call.ptr(), c.convs.ptr(), fmgr, c.aux));
        }
        t
    }

    pub fn ptr(&mut self) -> *mut Cell {
        self.buf.as_mut_ptr().cast()
    }

    /// The islands re-pointed at `c` after the compiled program moved to its final place (the cells' pointers are
    /// into heap buffers and survive the move; an island's program / state / binding pointers are to the value).
    pub fn rebind(&mut self, c: &mut Compiled<'_>) {
        for h in self.islands.iter_mut() {
            h.prog = &c.prog as *const Program<'_> as *const ();
            h.state = &mut c.state as *mut State as *mut ();
            h.params = &c.params as *const _ as *const ();
            h.callback_arg = c.callback_arg;
        }
    }

    /// These cells (`new`'s over a program with no interpreter island, before its lists moved: `Program::into_hosted`)
    /// re-pointed at `prog`'s lists where they are now, beside its `state` (D3438: the lowering backend's owner of a
    /// template, whose programs moved into the template's root): each site's call, argument forms and function-manager
    /// frame, and a synthetic site's descriptors or names (`aux`, as `of`).
    pub fn rebind_to(&mut self, prog: &HostedProgram, state: &mut State) {
        assert!(self.n == prog.sites.len() && self.islands.is_empty(), "EmitState::rebind_to over another program");
        self.repoint(&prog.view(), state.calls.as_mut_ptr(), &state.descs);
    }

    /// Each site's call, argument forms and function-manager frame in `prog` and `calls`, and the synthetic sites' `aux`
    /// (`rebind_to`, `rebind_all`).
    fn repoint(&mut self, prog: &Prog<'_, '_>, calls: *mut FmgrCall, descs: &[crate::nodes::tuple::Desc]) {
        let convs = prog.convs.as_ptr();
        for (i, call) in prog.sites.iter().enumerate() {
            let (convs, fmgr) = site_ptrs(convs, calls, call);
            let cell = self.cell_mut(i);
            (cell.call, cell.convs, cell.fmgr) = (word(call as *const Call), word(convs), word(fmgr));
        }
        self.auxes(prog, descs);
    }

    /// These cells (`new`'s over a program before its lists moved: `Compiled::into_in`) re-pointed at
    /// `c`'s lists where they are now: each site's call, argument forms and function-manager frame, a synthetic site's
    /// descriptors or names (`aux`, as `of`), and each island's program, state, binding and interpreter (`eval_hosted`).
    /// An island's `aux` (its boxed `IslandHost`) and a walked ScalarArrayOp's walk word are no list's address and stay;
    /// so do the error slots and results.
    pub fn rebind_all(&mut self, c: &mut HostedCompiled) {
        assert_eq!(self.n, c.prog.sites.len(), "EmitState::rebind_all over another program");
        self.repoint(&c.prog.view(), c.state.calls.as_mut_ptr(), &c.state.descs);
        for h in self.islands.iter_mut() {
            h.prog = &c.prog as *const HostedProgram as *const ();
            h.state = &mut c.state as *mut HostedState as *mut ();
            h.eval = eval_hosted;
            h.params = &c.params as *const _ as *const ();
            h.callback_arg = core::ptr::null_mut();
        }
    }

    /// The error a site's last call left (the replay's diagnosis).
    pub fn take_error(&mut self, site: usize) -> Option<Box<types_error::PgError>> {
        self.cell_mut(site).err.take()
    }
}

impl Drop for EmitState<'_> {
    fn drop(&mut self) {
        for i in 0..self.n {
            // SAFETY: cell `i` < `n` holds a header `push` wrote (its `ErrSlot` may own an error); dropped once.
            unsafe { core::ptr::drop_in_place(self.buf.as_mut_ptr().add(i * cell_size(self.width) / 8).cast::<Cell>()) };
        }
    }
}

gravity_abi::host! {
    /// The tier-B host: the shim over the cell's arguments (views formed in `region`); the status.
    /// # Safety — `cell` is a live `Cell` of an `EmitState` whose program is live; `region` a live `Region`.
    pub unsafe fn shim(cell: u64, region: u64) -> u64 {
        guarded(cell as *mut Cell, || {
            // a walked ScalarArrayOp (`Em::array_walk`): its operator site is a function's, so `aux` is the walk word there
            let c = &mut *(cell as *mut Cell);
            if c.aux != 0 && (*c.call.ptr()).funcid != 0 {
                let w = core::mem::take(&mut c.aux);
                return saop_walk(cell, region, w as u64);
            }
            shim_call(cell, region)
        })
    }
}

/// `shim`'s body, under `guarded`.
/// # Safety — as `shim`.
#[inline(always)]
unsafe fn shim_call(cell: u64, region: u64) -> u64 {
    let args = Cell::args(cell as *mut Cell);
    let c = &mut *(cell as *mut Cell);
    let call = &*c.call.ptr();
    let region = &mut *(region as *mut Region);
    let How::Shim { shim, byref, .. } = call.how else { return STATUS_ERROR as u64 };
    let n = call.nargs as usize;
    // D921: only the call's own arguments are formed; MAX_ARGS of them zeroed per call was a fifth of a hosted `->>` scan
    let mut a = [const { core::mem::MaybeUninit::<Arg>::uninit() }; MAX_ARGS];
    for i in 0..n {
        let conv = *c.convs.ptr().add(i);
        let d = Datum::from_i64(args[i]);
        a[i].write(match conv.kind {
            ArgKind::V | ArgKind::D => Arg::V(args[i]),
            ArgKind::P => match payload_view(d, conv.typlen, region) {
                Ok(v) => Arg::R(v),
                Err(e) => return fail(c, e),
            },
            ArgKind::I => match image_view(d, conv.typlen, region) {
                Ok(v) => Arg::R(v),
                Err(e) => return fail(c, e),
            },
        });
    }
    // SAFETY: `a[..n]` were written above (`Arg` is `Copy`: nothing to drop on an early return)
    let a = core::slice::from_raw_parts(a.as_ptr().cast::<Arg>(), n);
    let mut e = ErrSlot::NONE;
    let r = (shim.call)(&mut e, region, call.collation, &[], a);
    match r.status {
        gravity_fn::OK => {
            c.out = if byref { r.val as usize as i64 } else { r.val };
            STATUS_OK as u64
        }
        gravity_fn::NULL => STATUS_NULL as u64,
        gravity_fn::ERROR => fail(c, e.take().expect("a shim's ERROR carries its error")),
        _ => fail(c, crate::views::no_space()),
    }
}

gravity_abi::host! {
    /// The tier-C host: the function manager frame over the cell's arguments and null bytes.
    /// # Safety — as `shim`.
    pub unsafe fn fmgr(cell: u64) -> u64 {
        // D310: the called function can raise by unwinding (a nested SQL function's executor, its interrupt check under
        // statement_timeout): caught here, as `island`'s interpreter is, never across this `extern "C"` frame (an abort)
        guarded(cell as *mut Cell, || fmgr_call(cell as *mut Cell))
    }
}

/// `fmgr`'s body, under `guarded`.
/// # Safety — as `shim`.
#[inline(always)]
unsafe fn fmgr_call(cp: *mut Cell) -> u64 {
    // D3620: the null bytes follow the cell's `width` words (the program's widest site)
    let (args, nulls) = (Cell::args(cp), Cell::nulls(cp));
    let c = &mut *cp;
    let call = &*c.call.ptr();
    let n = (call.nargs as usize).min(args.len());
    // D1359: a pool lane's cells hold no function-manager frame (`arm_lane`); a call there is a named error, never a null deref
    let Some(f) = c.fmgr.ptr().as_mut() else { return fail(c, no_fmgr()) };
    // the cell's words go straight into the frame; a strict site's emitted code calls only over non-null arguments
    // (`scalar.rs` `call`: it stores no null bytes then), so the frame skips its null pass and strict re-check
    match f.call_site_words(&args[..n], &nulls[..n], call.strict) {
        Ok(v) if v.isnull => STATUS_NULL as u64,
        Ok(v) => {
            // D2311: an int2 / int4 word sign-extended (a hash body's is zero-extended, PG's `UInt32GetDatum`)
            c.out = crate::nodes::func::int_word(call.rettype, v.value.as_i64());
            STATUS_OK as u64
        }
        Err(e) => fail(c, e),
    }
}

/// The walk word the IR stores in the operator site's `Cell::aux` (`saop_walk` reads it back, `elem_of`): the element's
/// type (bits 0-31, never 0, so the word is not), its length (32-47), by value (48), `useOr` (49) and the alignment byte
/// (56-63).
pub fn walk_word(elem: ElemFacts, use_or: bool) -> i64 {
    (elem.typ as u64 | (elem.len as u16 as u64) << 32 | (elem.byval as u64) << 48 | (use_or as u64) << 49 | (elem.align as u8 as u64) << 56) as i64
}

/// `walk_word`'s element facts.
pub fn elem_of(w: u64) -> ElemFacts {
    ElemFacts { typ: w as u32, len: (w >> 32) as u16 as i16, byval: (w >> 48) & 1 != 0, align: (w >> 56) as u8 as i8 }
}

/// The walk (`Em::array_walk`, reached through `shim`): a ScalarArrayOp over an array the emitted code does not unroll (a
/// column, a parameter, a call's result, a longer ARRAY[...]), C `ExecEvalScalarArrayOp` over the array's elements, each
/// compared through the site's own cell (its shim or its function-manager frame: every operator the site binds, a
/// user-defined one, a record equality). The IR stores the scalar in `args[0]` and the array in `args[1]`, their NULLs in
/// `nulls[0]` / `nulls[1]`, and `word` is the walk word (`walk_word`: the element's facts, `useOr`). A NULL array is NULL; an
/// empty one false (ANY) / true (ALL) whatever the scalar; a NULL scalar of a strict operator NULL; else the elements in
/// order, decided at the first true (ANY) / false (ALL) answer, NULL when none decided and an element (of a strict
/// operator) or an answer was NULL. A raise ends the walk, the error in the site's cell (`fail`). The status as `shim`'s,
/// the answer (0 / 1) in `out`.
/// # Safety — as `shim`, under its `guarded`; the scalar / array words are datums of their types unless NULL.
unsafe fn saop_walk(cell: u64, region: u64, word: u64) -> u64 {
    // the cell and the region are reached through their pointers: the element's compare (`shim_call`) forms its own
    let (cp, rp) = (cell as *mut Cell, region as *mut Region);
    let (args, nulls) = (Cell::args(cp), Cell::nulls(cp));
    let (v, a) = (args[0] as u64, args[1] as u64);
    let (use_or, v_null, a_null) = ((word >> 49) & 1 != 0, nulls[0] != 0, nulls[1] != 0);
    let call = &*(*cp).call.ptr();
    if a_null {
        return STATUS_NULL as u64;
    }
    let mut w = match Walk::over(Datum::from_u64(a), elem_of(word), &mut *rp) {
        Ok(w) => w,
        Err(e) => return fail(&mut *cp, e),
    };
    // D1812: an empty array answers before the scalar's NULL is read
    if w.is_empty() {
        (*cp).out = !use_or as i64;
        return STATUS_OK as u64;
    }
    if v_null && call.strict {
        return STATUS_NULL as u64;
    }
    // an answer is a bool word: what a compare formed in the region (a view, a detoasted element) is released after it,
    // unless the region moved to a larger one meanwhile (D641's spill: the mark is the old region's)
    let (mark, end) = ((*rp).cur, (*rp).end);
    let mut null = false;
    while let Some(e) = w.next() {
        let st = if e.isnull && call.strict {
            STATUS_NULL
        } else {
            args[0] = v as i64;
            nulls[0] = v_null as u8;
            args[1] = e.value.as_i64();
            nulls[1] = e.isnull as u8;
            let st = match call.how {
                How::Shim { .. } => shim_call(cell, region),
                How::Fmgr(_) => fmgr_call(&mut *cp),
                How::Island { .. } => fail(&mut *cp, Box::new(types_error::PgError::error("gravity_expr: an array walk over an island site"))), // alloc: Cold
            } as i64;
            if (*rp).end == end {
                (*rp).cur = mark;
            }
            st
        };
        match st {
            STATUS_NULL => null = true,
            STATUS_OK if ((*cp).out != 0) == use_or => {
                (*cp).out = use_or as i64;
                return STATUS_OK as u64;
            }
            STATUS_OK => {}
            _ => return st as u64,
        }
    }
    if null {
        return STATUS_NULL as u64;
    }
    (*cp).out = !use_or as i64;
    STATUS_OK as u64
}

/// D1359: the error of a function-manager site reached on a lane that holds no frame for it (a pool lane).
#[cold]
fn no_fmgr() -> Box<types_error::PgError> {
    Box::new(types_error::PgError::error("gravity_expr: a function-manager call on a lane without its frame")) // alloc: Cold
}

gravity_abi::host! {
    /// The island host: the cell's arguments laid into the interpreter's row by source and column, the root evaluated
    /// by the interpreter in `region` (a by-reference result is an image there, as a shim's); the status as `shim`'s.
    /// # Safety — as `shim`; `cell.aux` is the island's `IslandHost` (`EmitState::new`), outliving the emit state.
    pub unsafe fn island(cell: u64, region: u64) -> u64 {
        let (args, cnulls) = (Cell::args(cell as *mut Cell), Cell::nulls(cell as *mut Cell));
        let c = &mut *(cell as *mut Cell);
        let call = &*c.call.ptr();
        let region = &mut *(region as *mut Region);
        let h = &mut *(c.aux as *mut IslandHost);
        // D901: a kept value of the same argument values
        if let Some((null, w)) = h.once.as_ref().and_then(|o| o.hit(args, cnulls, call.nargs as usize, region)) {
            if null {
                return STATUS_NULL as u64;
            }
            c.out = w;
            return STATUS_OK as u64;
        }
        let (prog, state, interp, params, callback_arg, root) = (h.prog, h.state, h.eval, h.params, h.callback_arg, h.root);
        let IslandHost { slots, values, nulls, prms, pext, pexec, aggnos, aggs, once, .. } = &mut *h;
        if let Some(o) = once.as_mut() {
            o.evals += 1;
        }
        for (i, &(src, attno)) in slots.iter().enumerate().take(call.nargs as usize) {
            let s = src as usize;
            values[s][attno as usize - 1] = Datum::from_i64(args[i]);
            nulls[s][attno as usize - 1] = cnulls[i] != 0;
        }
        for (j, &no) in aggnos.iter().enumerate() {
            let i = slots.len() + j;
            aggs[no as usize] = gravity_fn::NullableDatum { value: Datum::from_i64(args[i]), isnull: cnulls[i] != 0 };
        }
        for (j, &(exec, id, typ)) in prms.iter().enumerate() {
            let i = slots.len() + aggnos.len() + j;
            let (value, isnull) = (Datum::from_i64(args[i]), cnulls[i] != 0);
            match exec {
                true => pexec[id as usize] = types_portal::params::ParamExecData { value, isnull, exec_plan: false },
                false => pext[id as usize - 1] = types_portal::params::ParamExternData { value, isnull, pflags: 0, ptype: typ },
            }
        }
        let cols = |s: usize| Cols::Slot { values: &values[s], nulls: &nulls[s], types: &[], tdtypeid: 0, desc: None, slot: None };
        let row = Row { scan: cols(0), inner: cols(1), outer: cols(2), aggs: &aggs[..] };
        let bound;
        let outer = &*(params as *const types_portal::params::ParamBind<'static>);
        let params = match (pext.is_empty(), pexec.is_empty()) {
            (true, true) => outer,
            (ext, exec) => {
                // each list the island's own words, the other the statement's binding (V4R-454: executor parameters by id)
                let (extern_params, param_callback, param_expanded, param_rw, param_ro_expanded) = if ext {
                    (outer.extern_params, outer.param_callback, outer.param_expanded, outer.param_rw, outer.param_ro_expanded)
                } else {
                    (Some(&pext[..]), None, None, None, None)
                };
                let (exec_vals, n_exec) = if exec { (outer.exec_vals, outer.n_exec) } else { (core::ptr::NonNull::new(pexec.as_mut_ptr()), pexec.len() as u32) };
                bound = types_portal::params::ParamBind { extern_params, exec_vals, n_exec, param_callback, param_expanded, param_rw, param_ro_expanded, extern_fetch: if ext { outer.extern_fetch } else { None } };
                &bound
            }
        };
        let mut cx = Cx::new(&row, region, params, callback_arg);
        // an ereport under the interpreter unwinds: caught at this `extern "C"` frame, returned as the status word
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| interp(prog, state, root, &mut cx))); // unwind-ok: c-callback
        match r {
            Ok(Ok(v)) if v.isnull => {
                if let Some(o) = once.as_mut() {
                    o.keep(args, cnulls, call.nargs as usize, true, 0);
                }
                STATUS_NULL as u64
            }
            Ok(Ok(v)) => {
                // D2311: an int2 / int4 word sign-extended whatever node the interpreter answered from (a function-manager
                // call's or a parameter's word may be zero-extended); kept that way
                c.out = crate::nodes::func::int_word(call.rettype, v.value.as_i64());
                if let Some(o) = once.as_mut() {
                    o.keep(args, cnulls, call.nargs as usize, false, c.out);
                }
                STATUS_OK as u64
            }
            // an error is never kept (D901): the next call evaluates again
            Ok(Err(e)) => fail(c, e),
            Err(payload) => caught(c, payload),
        }
    }
}

/// A host call whose callee can raise by unwinding (an ereport as a panic carrying a `PgError`; a FATAL's thread exit): the
/// status is `STATUS_ERROR`, and an unwind never crosses the host's `extern "C"` frame, which would abort the process
/// (D310). The non-raising path is the call itself (catch_unwind is a landing pad).
/// # Safety — `c` is the live `Cell` the host was called with.
#[inline(always)]
unsafe fn guarded(c: *mut Cell, f: impl FnOnce() -> u64) -> u64 {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) { // unwind-ok: c-callback
        Ok(status) => status,
        Err(payload) => caught(&mut *c, payload),
    }
}

/// An unwind caught at a host frame: a `PgError` payload (boxed or not) is the site's error. Any other payload (a FATAL's
/// thread exit, a PANIC, a signal's kill, a bug's panic) is parked for the program's caller, which resumes it as soon as
/// the JIT'd frames have returned (`gravity_program::Code::call`), before anything reads the site's error: the cell holds
/// an internal error meanwhile, never reported.
#[cold]
#[inline(never)]
fn caught(c: &mut Cell, payload: Box<dyn std::any::Any + Send>) -> u64 {
    match payload.downcast::<Box<types_error::PgError>>() {
        Ok(e) => fail(c, *e),
        Err(payload) => match payload.downcast::<types_error::PgError>() {
            Ok(e) => fail(c, e),
            Err(payload) => {
                gravity_abi::status::unwind::park(payload);
                fail(c, Box::new(types_error::PgError::error("gravity: a host call's callee unwound (resumed at the program's return)".to_string()).with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR))) // alloc: Cold
            }
        },
    }
}

gravity_abi::host! {
    /// The direct road's raise: the shim's error moved from the call's stack slot into the site's cell (`fail`'s place).
    /// # Safety — `slot` is a live `Cell::err`; `err` is `Box::into_raw` of a shim's error.
    #[cold]
    #[inline(never)]
    pub unsafe fn slot_put(slot: u64, err: u64) {
        let s = &mut *(slot as *mut ErrSlot);
        drop(s.take());
        s.err = word(err as *mut types_error::PgError);
    }
}

gravity_abi::host! {
    /// D2834: a deferred extern parameter where the expression evaluates it (scalar.rs `deferred`): `$id` of the plan's
    /// type `typ` fetched from the execution's list `list` (nodes/param.rs `fetch_deferred`, C's ExecEvalParamExtern), its
    /// word into `out`; the status, an error stashed for the row's raise (`gravity_fn::stash_error`).
    /// # Safety — `list` is the running execution's parameter list (its `ParamNeed::Deferred` word); `out` a live word.
    #[guard(STATUS_ERROR as u64)]
    pub unsafe fn param_deferred(list: u64, id: u64, typ: u64, out: u64) -> u64 {
        match crate::nodes::param::fetch_deferred(types_portal::ParamListHandle(list), id as i32, typ as types_core::Oid) {
            Ok(v) if v.isnull => STATUS_NULL as u64,
            Ok(v) => {
                *(out as *mut i64) = v.value.as_i64();
                STATUS_OK as u64
            }
            Err(e) => {
                gravity_fn::stash_error(e);
                STATUS_ERROR as u64
            }
        }
    }
}

#[cold]
#[inline(never)]
fn fail(c: &mut Cell, e: Box<types_error::PgError>) -> u64 {
    drop(c.err.take());
    c.err.err = word(Box::into_raw(e));
    STATUS_ERROR as u64
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use gravity_fn::{Addr, Arg, ArgKind, ErrSlot, Family, FmgrCall, Region, Ret, Shim};

    use super::{fmgr, shim, Cell, EmitState, STATUS_ERROR, STATUS_OK};
    use crate::nodes::func::{ArgConv, Call, How, MAX_ARGS};
    use crate::Kids;

    static SEEN: AtomicUsize = AtomicUsize::new(0);

    /// The test shim: records how many arguments it was handed and sums them (each a word).
    fn sum(_: *mut ErrSlot, _: *mut Region, _: u32, _: &[u32], a: &[Arg]) -> Ret {
        SEEN.store(a.len(), Ordering::SeqCst);
        let val = a.iter().map(|x| if let Arg::V(v) = *x { v } else { i64::MIN }).fold(0i64, i64::wrapping_add);
        Ret { val, status: gravity_fn::OK }
    }
    unsafe extern "C-unwind" fn nop() {}
    static KINDS: [ArgKind; MAX_ARGS] = [ArgKind::V; MAX_ARGS];
    static SUM: Shim = Shim { oid: 0, name: "d921_sum", family: Family::ByVal, nargs: MAX_ARGS as u8, coll: false, region: false, args: &KINDS, tyargs: &[], addr: Addr(nop), call: sum };

    /// D921: `shim` hands the callee exactly the call's `nargs` formed arguments, a slice of that length holding each word
    /// in order: at the most a call takes (MAX_ARGS), at 2 and at none. The frame's unformed tail is never in it.
    /// A context for the tests' cells (leaked: the cells outlive the test's frame).
    fn test_cx() -> mcx::Mcx<'static> {
        Box::leak(Box::new(mcx::MemoryContext::new("emit test"))).mcx() // alloc: Cold
    }

    /// One cell `width` wide over `head`, its words `args` (the rest zero).
    fn one(width: usize, head: Cell, args: &[i64]) -> EmitState<'static> {
        let mut st = EmitState::with_cells(1, width, test_cx());
        st.push(head);
        // SAFETY: the state's one cell, live while `st` is
        unsafe { Cell::args(st.ptr())[..args.len()].copy_from_slice(args) };
        st
    }

    #[test]
    fn a_shim_sees_exactly_its_nargs_arguments() {
        // D3620: in a cell as wide as the call (2) and in a wider program's cell (MAX_ARGS) alike
        for (n, width) in [(MAX_ARGS, MAX_ARGS), (2, 2), (2, MAX_ARGS), (0, 0), (0, MAX_ARGS)] {
            let how = How::Shim { shim: &SUM, convs: 0, byref: false };
            let call = Call { funcid: 0, args: Kids { start: 0, len: n as u32 }, nargs: n as u8, strict: false, collation: 0, how, rettype: 20, volatile: false };
            let convs = [ArgConv { kind: ArgKind::V, typlen: 8 }; MAX_ARGS];
            let args: Vec<i64> = (0..width as i64).map(|i| 3 * i + 1).collect();
            let mut st = one(width, Cell::head(&call, convs.as_ptr(), core::ptr::null_mut(), 0), &args);
            let mut scratch = [0u8; 64];
            let mut region = Region::over(&mut scratch);
            SEEN.store(usize::MAX, Ordering::SeqCst);
            // SAFETY: the cell, its call and convs, and the region are live across the call
            let s = unsafe { shim(st.ptr() as u64, &mut region as *mut Region as u64) };
            assert_eq!(s as i64, STATUS_OK, "{n} arguments");
            assert_eq!(SEEN.load(Ordering::SeqCst), n, "the callee's slice is the call's nargs");
            assert_eq!(st.cell(0).out, (0..n as i64).map(|i| 3 * i + 1).sum::<i64>(), "each word in order");
        }
    }

    /// D3620: a cell holds its program's width of words and null bytes (`cell_size`), the null bytes right after the words
    /// (`Cell::nulls`, what the function-manager host reads), and the cells of one state follow each other at that size.
    #[test]
    fn cells_are_as_wide_as_their_program() {
        assert_eq!(super::cell_size(2), super::CELL_ARGS as usize + 24, "2 words + 2 null bytes, to a word");
        assert_eq!(super::cell_size(MAX_ARGS), super::CELL_ARGS as usize + 904);
        let mut st = EmitState::with_cells(3, 2, test_cx());
        for aux in 1..=3 {
            st.push(Cell::head(core::ptr::null(), core::ptr::null(), core::ptr::null_mut(), aux));
        }
        let base = st.ptr() as usize;
        for i in 0..3 {
            let c = (base + i * super::cell_size(2)) as *mut Cell;
            // SAFETY: cell `i` of `st`, live
            unsafe {
                assert_eq!(((*c).aux, (*c).width), (i as i64 + 1, 2), "cell {i} at its stride");
                assert_eq!(Cell::nulls(c).as_ptr() as usize, c as usize + super::cell_nulls(2) as usize);
                Cell::args(c).copy_from_slice(&[10 * i as i64, 10 * i as i64 + 1]);
                Cell::nulls(c).copy_from_slice(&[1, 0]);
            }
        }
        for i in 0..3 {
            let c = (base + i * super::cell_size(2)) as *mut Cell;
            // SAFETY: as above: no cell's words or null bytes reach the next cell's header
            unsafe { assert_eq!((Cell::args(c)[1], Cell::nulls(c)[0], (*c).aux), (10 * i as i64 + 1, 1, i as i64 + 1)) };
        }
    }

    /// D2311: a function-manager call's int4 result reaches the cell sign-extended, the word the emitted code reads whole
    /// (prim.rs). pgrust's hashint4 wrapper returns `Datum::from_u32` (PG's `UInt32GetDatum`: zero-extended), so -8..8 give
    /// negative values whose frame word is 2^32 + h. DOES NOT APPLY: a site whose result type is not int2 / int4 (int8
    /// here) keeps the callee's word as it is.
    #[test]
    fn a_function_manager_int4_result_is_sign_extended() {
        let (mut negative, mut zext) = (0, 0);
        for (rettype, v) in (-8i64..8).map(|v| (23u32, v)).chain((-8i64..8).map(|v| (20u32, v))) {
            let mut f = gravity_fn::FmgrCall::bind(450, 1, 0).expect("hashint4 binds");
            let call = Call { funcid: 450, args: Kids { start: 0, len: 1 }, nargs: 1, strict: true, collation: 0, how: How::Fmgr(0), rettype, volatile: false };
            let mut cs = one(1, Cell::head(&call, core::ptr::null(), &mut f as *mut FmgrCall, 0), &[v]);
            // SAFETY: the cell, its call and its frame are live across the call
            let st = unsafe { super::fmgr(cs.ptr() as u64) };
            assert_eq!(st as i64, STATUS_OK, "hashint4({v})");
            let out = cs.cell(0).out;
            let h = out as i32;
            if rettype == 23 {
                assert_eq!(out, h as i64, "hashint4({v}): the int4 word is sign-extended");
                negative += (h < 0) as usize;
            } else {
                assert_eq!(out, h as u32 as i64, "hashint4({v}) as a non-int4 site: the callee's word as it is");
                zext += (h < 0) as usize;
            }
        }
        assert!(negative > 0 && zext > 0, "the inputs reach a negative hash ({negative} / {zext})");
    }

    /// D1359: a function-manager site whose cell holds no frame (a pool lane's, `arm_lane`) answers a named error, not a null deref.
    #[test]
    fn a_fmgr_site_without_its_frame_is_a_named_error() {
        let how = How::Shim { shim: &SUM, convs: 0, byref: false };
        let call = Call { funcid: 0, args: Kids { start: 0, len: 1 }, nargs: 1, strict: false, collation: 0, how, rettype: 20, volatile: false };
        let convs = [ArgConv { kind: ArgKind::V, typlen: 8 }; MAX_ARGS];
        let mut cs = one(1, Cell::head(&call, convs.as_ptr(), core::ptr::null_mut(), 0), &[7]);
        // SAFETY: the cell and its call are live across the call; the null frame is the case under test
        let st = unsafe { fmgr(cs.ptr() as u64) };
        assert_eq!(st as i64, STATUS_ERROR);
        let e = cs.take_error(0).expect("the site's error");
        assert!(e.message().contains("function-manager call on a lane without its frame"), "{}", e.message());
    }
}

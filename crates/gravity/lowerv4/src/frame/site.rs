//! The site-window reader (M1-SOURCE, moved from source/pgrc.rs so later sources share it). Owner M1-CORE.
//!
//! One window loop over a bound site: the open seeks (an index site binds its key words into the site's key cells
//! first), `advance` stages the next window, and a row's cells are the site's attribute lanes at the row's index through
//! the window's selection, each cell's value its element's address; a nullable lane's NULL bit is read branch-free from
//! its validity words. The pgrc source (source/pgrc.rs) and the index source (source/index.rs) read through it.
use core::cell::{Cell as Once, RefCell};

use gravity_abi::ctx::{lane, site_off};
use gravity_abi::site::site;
use gravity_expr::emit::LaneRef;
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{Block, ConvOp, HostFlags, HostFn, IntBinOp, IntCond, Type, Value};
use gravity_kernels::host_abi as abi;
use gravity_program::{ArenaVec, ExprLife, ExprOut, LaneDesc, LaneId as SiteLane, OutCol, SiteDesc, SiteKind};

use mcx::{CollectIn, GrowIn, Mcx, PgRc, PgVec};

use crate::cell::{Cell, LaneId, Nullable, Row, TIDOID};
use crate::face::source::Locator;
use crate::frame::env::Env;
use crate::frame::frame::WordWin;
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::admit::refusal::{Reason, ShapeWhy};

/// One attribute lane of the site: its index, attribute, SQL type, whether the relation declares it NOT NULL.
#[derive(Clone, Copy)]
pub(crate) struct Col {
    pub k: usize,
    pub attno: i16,
    pub typid: u32,
    pub notnull: bool,
    pub desc: LaneDesc,
}

/// The window's values the rows read (loaded once per window): the selection, `(ptr, valid, valid is null)` per column,
/// the lane array, an index window's position words (the row's tid).
#[derive(Clone)]
pub(crate) struct WinVals<'g> {
    pub sel: Value,
    pub sel_null: Value,
    pub lanes: PgRc<'g, [(Value, Value, Value)]>,
    pub lane_arr: Value,
    tid: Option<(Value, Value)>,
}

/// `ctid` (PG's `SelfItemPointerAttributeNumber`) renumbered past every attribute: the row's `tid` cell's column.
pub(crate) const CTID_AT: i16 = 1602;

/// M6 D2: shred lane `n` of the site (`SiteDesc::shreds`) is the row's column `SHRED_AT + n`, past every extension site's
/// placeholder (`face/ext.rs` `EXT_AT`).
pub(crate) const SHRED_AT: i16 = 1800;

/// A key cell the host fills itself (a constant `= ANY` array): the seek leaves it.
pub(crate) const HOST_KEY: u16 = u16::MAX;

/// M8 L0-a: `EXEC_KEY | id` keys a cell by executor parameter `id`, read from its words at every (re-)open; `INIT_KEY` too
/// by an InitPlan's, read through `params::InitLoad` (params.rs `exec_key`).
pub(crate) const EXEC_KEY: u16 = 0x8000;
pub(crate) const INIT_KEY: u16 = 0x4000;

/// The site's window loop; an index site binds its key words into the site's key cells before the seek.
pub(crate) struct Reader<'g> {
    mcx: Mcx<'g>,
    pub cols: PgVec<'g, Col>,
    natts: usize,
    pub node: i32,
    keys: Option<PgVec<'g, u16>>,
    /// A ranges site (`SiteDesc::ranges`): the key cell its claim starts at, past the key words (the claim's `lo` there,
    /// its `hi` in the next), stored before each seek as `range_seek` stores a range site's.
    claim_at: Option<usize>,
    /// M8 L0-a: each key's InitPlan handle word (`params::init_handle`, 0 for another key); empty without an InitPlan key.
    inits: PgVec<'g, u16>,
    /// An index window's position is its one heap row's tid (`POS_BLOCK` / `POS_OFF`): the locator, and `ctid` as the
    /// column `ctid` names (`CTID_AT`).
    tid: bool,
    ctid: Option<i16>,
    /// An index site that keeps a RowId lane (the host's N-row windows, V4R-55): each row's tid is that lane's word
    /// (hosts `tid_word`), read per row by `ctid` and the locator; `row_i` is the current row's window index.
    rowid: Option<usize>,
    row_i: Once<Option<Value>>,
    hosts: Once<Option<(HostFn, HostFn)>>,
    win: RefCell<Option<WinVals<'g>>>,
}

impl<'g> Reader<'g> {
    pub(crate) fn new(px: &Px<'g, '_>, site: &SiteDesc, relid: u32, node: i32, keys: Option<PgVec<'g, u16>>, ctid: Option<i16>) -> Reader<'g> {
        let mcx = px.mcx();
        let mut cols = PgVec::new_in(mcx);
        for (i, l) in site.lanes().iter().enumerate() {
            // M6 D2: a shredded jsonb path lane is the row's column `SHRED_AT + n` (step/jsonb_inplace.rs reads it)
            if let (SiteLane::JsonbPath(n), Some(d)) = (l.id, site.shred_of(i)) {
                // P1b: a number path's lane is int8 (strategies/jsonb_nums.rs); jsonops (3): a presence lane is a boolean word
                let typid = match d.kind {
                    gravity_program::shred_kind::NUMERIC_FS => types_core::INT8OID,
                    // D3038 (R2): an emptiness lane is `col <> ''`'s boolean word (strategies/octet_lane.rs)
                    gravity_program::shred_kind::PRESENCE | gravity_program::shred_kind::NONEMPTY => types_core::BOOLOID,
                    // D3038: an octet lane is `octet_length`'s int4 (strategies/octet_lane.rs)
                    gravity_program::shred_kind::OCTET_LENGTH | gravity_program::shred_kind::CHAR_LENGTH => types_core::INT4OID,
                    _ => types_core::TEXTOID,
                };
                cols.push_in(Col { k: i, attno: SHRED_AT + n as i16, typid, notnull: false, desc: *l });
                continue;
            }
            let SiteLane::Attr(a) = l.id else { continue };
            // V4R-88: a heap system lane (xmin / xmax: xid, cmin / cmax: cid), never null
            if (gravity_program::protos::XMIN_AT..=gravity_program::protos::CMAX_AT).contains(&a) {
                let typid = if a == gravity_program::protos::XMIN_AT || a == gravity_program::protos::XMAX_AT { types_core::XIDOID } else { types_core::CIDOID };
                cols.push_in(Col { k: i, attno: a, typid, notnull: true, desc: *l });
                continue;
            }
            // D2789: a dropped attribute a whole row reads (decompose `whole_row_atts`) is its stored datum, with no type
            let Some((typid, notnull)) = px.gx.facts.att(relid, a).map(|f| (f.typid, f.notnull)).or_else(|| px.gx.facts.dropped_layout(relid, a).map(|_| (0, false))) else {
                crate::refuse!(node, Reason::Shape { why: ShapeWhy("a scan column without catalog facts") })
            };
            cols.push_in(Col { k: i, attno: a, typid, notnull, desc: *l });
        }
        // V4R-88: the system columns' attnos (XMIN_AT .. CMAX_AT) sit above CTID_AT, so the widest of both
        let natts = cols.iter().map(|c| c.attno as usize).chain(ctid.map(|c| c as usize)).max().unwrap_or(0);
        let tid = keys.is_some();
        // D1016: a host key (`HOST_KEY`, every bit set) is no InitPlan key
        let init = |&w: &u16| (w != HOST_KEY && w & EXEC_KEY != 0 && w & INIT_KEY != 0).then(|| crate::rerun::params::init_handle_of(px, (w & !(EXEC_KEY | INIT_KEY)) as i32));
        let inits = match keys.iter().flatten().any(|w| init(w).is_some()) {
            true => keys.iter().flatten().map(|w| init(w).map_or(0, |h| h.unwrap_or_else(|| px.bug("an InitPlan key without its handle word")))).collect_in(mcx),
            false => PgVec::new_in(mcx),
        };
        let rowid = site.lanes().iter().position(|l| l.id == SiteLane::RowId).filter(|_| tid);
        let claim_at = site.ranges().then(|| site.key_words());
        Reader { mcx, cols, natts, node, keys, claim_at, inits, tid, ctid, rowid, row_i: Once::new(None), hosts: Once::new(None), win: RefCell::new(None) }
    }

    pub(crate) fn k(&self, env: &Env) -> usize {
        match env.site {
            Some(k) => k,
            None => crate::refuse!(self.node, Reason::Bug { payload: "a site source without its site" }),
        }
    }

    fn hosts(&self) -> (HostFn, HostFn) {
        match self.hosts.get() {
            Some(h) => h,
            None => crate::refuse!(self.node, Reason::Bug { payload: "a site source lowered before its declare" }),
        }
    }

    pub(crate) fn declare(&self, pb: &mut ProgramBuilder) {
        let mem = HostFlags { touches_memory: true, ..HostFlags::default() };
        let ps = &[Type::Ptr, Type::I64];
        let seek = match self.keys {
            Some(_) => pb.declare_host("seek", ps, Some(Type::I64), gravity_kernels::contract::entry1::<abi::Seek> as *const () as usize, mem),
            None => pb.declare_host("range_seek", ps, Some(Type::I64), gravity_kernels::contract::entry1::<abi::RangeSeek> as *const () as usize, mem),
        };
        let advance = pb.declare_host("advance", ps, Some(Type::I64), gravity_kernels::contract::entry1::<abi::Advance> as *const () as usize, mem);
        self.hosts.set(Some((seek, advance)));
    }

    pub(crate) fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        if let Some(keys) = &self.keys {
            let k = self.k(env);
            store_keys(b, env, k, keys, &self.inits);
        }
        if let Some(at) = self.claim_at {
            store_claim(b, env, self.k(env), at);
        }
        let k = b.iconst(Type::I64, self.k(env) as i64);
        b.call_host(self.hosts().0, &[env.ctx, k], Some(Type::I64)).unwrap_or(env.zero)
    }

    /// The next window `[cursor, end)` (`l_eof(status)` at the end or on a stop), its lanes loaded for the rows.
    pub(crate) fn window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> (Value, Value) {
        let k = self.k(env);
        let kv = b.iconst(Type::I64, k as i64);
        let r = b.call_host(self.hosts().1, &[env.ctx, kv], Some(Type::I64)).unwrap_or(env.zero);
        let le = b.icmp(IntCond::Sle, r, env.zero);
        let l_window = b.create_block(&[]);
        b.cond_br(le, (l_eof, &[r]), (l_window, &[]));
        b.switch_to(l_window);
        let s = site_off(k) as i32;
        let cursor = load_u32(b, env, s + site::CURSOR as i32);
        let rows = load_u32(b, env, s + site::ROWS as i32);
        let sel = b.load(Type::Ptr, env.ctx, s + site::SEL as i32);
        let sel_i = b.conv(ConvOp::PtrToInt, Type::I64, sel);
        let sel_null = b.icmp(IntCond::Eq, sel_i, env.zero);
        let sel_n = load_u32(b, env, s + site::SEL_N as i32);
        let end = b.select(sel_null, rows, sel_n);
        let lane_arr = b.load(Type::Ptr, env.ctx, s + site::LANES as i32);
        let mut lv = mcx::reserved_in(self.cols.len(), self.mcx);
        for c in self.cols.iter() {
            let at = (c.k * lane::SIZE) as i32;
            let ptr = b.load(Type::Ptr, lane_arr, at + lane::PTR as i32);
            let valid = b.load(Type::Ptr, lane_arr, at + lane::VALID as i32);
            let vi = b.conv(ConvOp::PtrToInt, Type::I64, valid);
            let vnull = b.icmp(IntCond::Eq, vi, env.zero);
            lv.push_in((ptr, valid, vnull));
        }
        let tid = self.tid.then(|| {
            let block = b.load(Type::I32, env.ctx, s + site::POS_BLOCK as i32);
            (block, b.load(Type::I16, env.ctx, s + site::POS_OFF as i32))
        });
        *self.win.borrow_mut() = Some(WinVals { sel, sel_null, lanes: PgRc::from_slice_in(&lv[..], self.mcx), lane_arr, tid });
        (cursor, end)
    }

    pub(crate) fn vals(&self) -> WinVals<'g> {
        match self.win.borrow().clone() {
            Some(w) => w,
            None => crate::refuse!(self.node, Reason::Bug { payload: "a site row before its window" }),
        }
    }

    /// Row `j`'s index in the window: itself (no selection, or a body without one), or the selection's entry `j`.
    pub(crate) fn row_index(&self, b: &mut FunctionBuilder<'_>, env: &Env, j: Value) -> Value {
        if env.body.nosel {
            return j;
        }
        let w = self.vals();
        let l_gather = b.create_block(&[]);
        let l_row = b.create_block(&[Type::I64]);
        b.cond_br(w.sel_null, (l_row, &[j]), (l_gather, &[]));
        b.switch_to(l_gather);
        let ip = b.ptr_add(w.sel, j, 4, 0);
        let i32v = b.load(Type::I32, ip, 0);
        let i = b.conv(ConvOp::Zext, Type::I64, i32v);
        b.br(l_row, &[i]);
        b.switch_to(l_row);
        b.block_params(l_row)[0]
    }

    pub(crate) fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        let w = self.vals();
        let i = self.row_index(b, env, j);
        self.row_i.set(Some(i));
        let mut cells = mcx::reserved_in(self.cols.len() + 1, env.mcx);
        let mut vals = mcx::reserved_in(self.cols.len() + 1, env.mcx);
        let mut cols = mcx::pg_vec![in env.mcx; None; self.natts];
        for (n, (c, &(ptr, valid, vnull))) in self.cols.iter().zip(w.lanes.iter()).enumerate() {
            let addr = b.ptr_add(ptr, i, c.desc.width, 0);
            let null = match (c.notnull, env.body.nonulls) {
                (true, _) => None,
                (false, true) => Some(b.bconst(false)),
                (false, false) => Some(null_bit(b, env, i, valid, vnull)),
            };
            let nl = if c.notnull { Nullable::Never } else { Nullable::Maybe };
            cells.push_in(Cell::of_lane(LaneId::col(c.k as u16), &c.desc, c.typid, nl));
            vals.push_in((addr, null));
            cols[c.attno as usize - 1] = Some(n as u16);
        }
        let tid = self.ctid.and_then(|_| self.rowid.map_or(w.tid, |r| Some(row_tid(b, &w, r, i))));
        if let (Some((block, off)), Some(at)) = (tid, self.ctid) {
            // the ItemPointerData image (bi_hi, bi_lo, ip_posid) in a row-lived slot: the out row copies a by-reference cell
            let slot = b.declare_stack_slot(8, 8);
            let img = b.stack_addr(slot);
            let sixteen = b.iconst(Type::I32, 16);
            let hi = b.int_bin(IntBinOp::Ushr, block, sixteen);
            let lo = b.int_bin(IntBinOp::Shl, block, sixteen);
            let bi = b.int_bin(IntBinOp::Or, hi, lo);
            b.store(img, 0, bi);
            b.store(img, 4, off);
            let out = ExprOut { rep: OutCol::new("", TIDOID, -1, 6, false).rep, typid: TIDOID, life: ExprLife::Row };
            cols[at as usize - 1] = Some(cells.len() as u16);
            cells.push_in(Cell::of_expr(out, cells.len() as u16, 0, Nullable::Never));
            vals.push_in((b.conv(ConvOp::PtrToInt, Type::I64, img), None));
        }
        RowVals { row: Row { cells }, vals, cols, carry, next, at: j }
    }

    /// The window's by-value lanes (2, 4 or 8 bytes wide) by column for the 64-row block path (H01; KEEP v3-r1-a4
    /// f416a2c2682), each nullable lane's validity words.
    pub(crate) fn word_win<'e>(&self, mcx: Mcx<'e>) -> Option<WordWin<'e>> {
        let win = self.win.borrow();
        let w = win.as_ref()?;
        let (mut lanes, mut valid) = (mcx::pg_vec![in mcx; None; self.natts], mcx::pg_vec![in mcx; None; self.natts]);
        for (c, &(ptr, v, vnull)) in self.cols.iter().zip(w.lanes.iter()) {
            if c.desc.kind != lane::KIND_BYVAL || !matches!(c.desc.width, 2 | 4 | 8) {
                continue;
            }
            let at = c.attno as usize - 1;
            lanes[at] = Some(LaneRef { ptr, valid: v, valid_null: None, width: c.desc.width, kind: c.desc.kind, all_valid: true });
            valid[at] = (!c.notnull).then_some((v, vnull));
        }
        Some(WordWin { lanes, valid, plain: w.sel_null })
    }

    /// No lane the rows read has a null in the window (every nullable lane without a validity bitmap); `None` when every
    /// lane is NOT NULL, where the fact would buy nothing.
    pub(crate) fn nonulls(&self, b: &mut FunctionBuilder<'_>) -> Option<Value> {
        let w = self.vals();
        let mut all: Option<Value> = None;
        for (c, &(_, _, vnull)) in self.cols.iter().zip(w.lanes.iter()) {
            if !c.notnull {
                all = Some(match all {
                    None => vnull,
                    Some(a) => b.int_bin(IntBinOp::And, a, vnull),
                });
            }
        }
        all
    }

    /// The row's heap locator (H28): the index window's position `(block, offset)`, or the row's own tid in an N-row window.
    pub(crate) fn locator(&self, b: &mut FunctionBuilder<'_>) -> Option<Locator> {
        let w = self.vals();
        let (block, off) = match (self.rowid, self.row_i.get()) {
            (Some(r), Some(i)) => row_tid(b, &w, r, i),
            _ => w.tid?,
        };
        let block = b.conv(ConvOp::Zext, Type::I64, block);
        let offset = b.conv(ConvOp::Zext, Type::I64, off);
        Some(Locator { block, offset, pinned: None, rel: None })
    }
}

/// Row `i`'s tid from the RowId lane `r` (hosts `tid_word`: block << 16 | offset) as the window position's types.
pub(crate) fn row_tid(b: &mut FunctionBuilder<'_>, w: &WinVals<'_>, r: usize, i: Value) -> (Value, Value) {
    let ptr = b.load(Type::Ptr, w.lane_arr, (r * lane::SIZE) as i32 + lane::PTR as i32);
    let at = b.ptr_add(ptr, i, 8, 0);
    let word = b.load(Type::I64, at, 0);
    let sixteen = b.iconst(Type::I64, 16);
    let block = b.int_bin(IntBinOp::Ushr, word, sixteen);
    (b.conv(ConvOp::Trunc, Type::I32, block), b.conv(ConvOp::Trunc, Type::I16, word))
}

fn load_u32(b: &mut FunctionBuilder<'_>, env: &Env, off: i32) -> Value {
    let v = b.load(Type::I32, env.ctx, off);
    b.conv(ConvOp::Zext, Type::I64, v)
}

/// The site's key cells from the bind's words: `keys[n] = params[slots[n]]` with its null byte, or an executor
/// parameter's words (`EXEC_KEY`), an InitPlan's through its handle word `inits[n]` (`INIT_KEY`); a host key is left.
fn store_keys(b: &mut FunctionBuilder<'_>, env: &mut Env, k: usize, slots: &[u16], inits: &[u16]) {
    let s = site_off(k) as i32;
    let keys = b.load(Type::Ptr, env.ctx, s + site::KEYS as i32);
    let keys_null = b.load(Type::Ptr, env.ctx, s + site::KEYS_NULL as i32);
    for (n, &w) in slots.iter().enumerate() {
        if w == HOST_KEY {
            continue;
        }
        let (v, isnull) = match (w & EXEC_KEY, w & INIT_KEY) {
            (0, _) => (b.load(Type::I64, env.params, w as i32 * 8), b.load(Type::I8, env.params_null, w as i32)),
            (_, 0) => {
                let (v, null) = crate::rerun::params::load(b, env, env.node, (w & !EXEC_KEY) as i32);
                (v, b.conv(ConvOp::Zext, Type::I8, null))
            }
            _ => init_key(b, env, (w & !(EXEC_KEY | INIT_KEY)) as i32, inits.get(n).copied().unwrap_or(0)),
        };
        b.store(keys, n as i32 * 8, v);
        b.store(keys_null, n as i32, isnull);
    }
}

/// A ranges site's claim (the context's `claim`) into its key cells `at` (`lo`) and `at + 1` (`hi`), never null.
fn store_claim(b: &mut FunctionBuilder<'_>, env: &mut Env, k: usize, at: usize) {
    use gravity_abi::ctx::hdr;
    let s = site_off(k) as i32;
    let keys = b.load(Type::Ptr, env.ctx, s + site::KEYS as i32);
    let keys_null = b.load(Type::Ptr, env.ctx, s + site::KEYS_NULL as i32);
    let lo = b.load(Type::I64, env.ctx, hdr::CLAIM_LO as i32);
    let hi = b.load(Type::I64, env.ctx, hdr::CLAIM_HI as i32);
    let zero = b.iconst(Type::I8, 0);
    b.store(keys, at as i32 * 8, lo);
    b.store(keys, (at + 1) as i32 * 8, hi);
    b.store(keys_null, at as i32, zero);
    b.store(keys_null, (at + 1) as i32, zero);
}

/// An InitPlan parameter's key cell (`INIT_KEY`): nest.initplan's `InitLoad` runs it here if this context has not, and an
/// error raises from the open. Also a pgrc site's open-time zone comparand (PS1-05, pgrc/lower.rs `izone_keys`).
pub(crate) fn init_key(b: &mut FunctionBuilder<'_>, env: &mut Env, id: i32, h: u16) -> (Value, Value) {
    let node = env.node;
    let load = crate::frame::iface::kind_impl::<crate::rerun::params::InitLoad>(&node, crate::face::kinds::KindId::NestInitplan).0;
    let [v, valid, err] = load(b, env, node, h, id);
    let raised = b.icmp(IntCond::Ne, err, env.zero);
    let l_ok = b.create_block(&[]);
    b.cond_br(raised, (env.l_raise, &[err]), (l_ok, &[]));
    b.switch_to(l_ok);
    let (no, yes) = (b.iconst(Type::I8, 0), b.iconst(Type::I8, 1));
    (v, b.select(valid, no, yes))
}

/// Row `i`'s NULL flag, branch-free: a lane with no bitmap is all valid (its word is read from the context instead).
fn null_bit(b: &mut FunctionBuilder<'_>, env: &Env, i: Value, valid: Value, vnull: Value) -> Value {
    let six = b.iconst(Type::I64, 6);
    let wi = b.int_bin(IntBinOp::Ushr, i, six);
    let wp = b.ptr_add(valid, wi, 8, 0);
    let at = b.select(vnull, env.ctx, wp);
    let word = b.load(Type::I64, at, 0);
    let m63 = b.iconst(Type::I64, 63);
    let sh = b.int_bin(IntBinOp::And, i, m63);
    let bit = b.int_bin(IntBinOp::Ushr, word, sh);
    let bit1 = b.int_bin(IntBinOp::And, bit, env.one);
    let unset = b.icmp(IntCond::Eq, bit1, env.zero);
    let t = b.bconst(true);
    let has = b.int_bin(IntBinOp::Xor, vnull, t);
    b.int_bin(IntBinOp::And, unset, has)
}

/// An index source's site (index.rs, M2's index sources): the decomposer's site, one key cell per key word (`attnos` its
/// table columns, 0 an expression column), by-value lanes as the host's 8-byte datum words, a scan reading no column driven
/// by the relation's first by-value column (else its first live column, D607); the rowid lane dropped (`ctid`, the second result, is the window's position).
pub(crate) fn index_site(px: &Px<'_, '_>, d: &SiteDesc, relid: u32, slots: &[u16], attnos: &[i16], node: i32) -> (SiteDesc, bool) {
    let attrs = d.lanes().iter().any(|l| matches!(l.id, SiteLane::Attr(_)));
    let mut s = if attrs {
        d.copy_in(px.arena)
    } else {
        // D607: a relation with no by-value column (an RI check's `SELECT 1 FROM ONLY p WHERE k = $1` over a text key) is
        // driven by its first live column, a by-reference lane whose datum nothing reads
        let byval = |a: i16| px.gx.facts.att(relid, a).is_some_and(|f| f.byval);
        let atts = 1..=px.gx.facts.rel(relid).map_or(0, |r| r.natts);
        let Some(a) = atts.clone().find(|&a| byval(a)).or_else(|| atts.clone().find(|&a| px.gx.facts.att(relid, a).is_some())) else {
            crate::refuse!(node, Reason::Shape { why: ShapeWhy("an index scan reading no column over a relation with no live column to drive it") })
        };
        SiteDesc::index_source(px.arena, d.relation, d.index, &d.key_attnos, &d.param_slots, &[a])
    };
    let ctid = d.lanes().iter().any(|l| l.id == SiteLane::RowId);
    if s.lanes().last().is_some_and(|l| l.id == SiteLane::RowId) {
        s.lanes.pop();
    }
    let mut ps = ArenaVec::with_capacity_in(slots.len(), px.arena);
    ps.extend(slots.iter().map(|&w| if w & EXEC_KEY != 0 { -1 } else { w as i16 }));
    s.param_slots = ps;
    let mut ka = ArenaVec::with_capacity_in(attnos.len(), px.arena);
    ka.extend_from_slice(attnos);
    s.key_attnos = ka;
    let SiteKind::IndexSource { natts, worker, leaves, ranges, .. } = s.kind else { crate::refuse!(node, Reason::Bug { payload: "an index site of another kind" }) };
    // a ranges site's claim rides in `RANGE_KEYS` cells past the key words (`Reader::claim_at`)
    let claim = if ranges { gravity_program::RANGE_KEYS } else { 0 };
    s.kind = SiteKind::IndexSource { nkeys: (slots.len() + claim) as u16, natts, worker, leaves, ranges };
    for l in s.lanes.iter_mut() {
        if matches!(l.id, SiteLane::Attr(_)) && l.kind == lane::KIND_BYVAL {
            l.width = 8;
        }
    }
    s.filter_lanes.clear();
    (s, ctid)
}

#[cfg(test)]
mod d607 {
    use gravity_decompose::{Am, AttFact, ExprFp, Facts, IndexFact, PipelineGraph, PipelineNode, RelFact, Source};
    use gravity_lower::Sequential;
    use gravity_program::{Arena, End, LaneId, Pipeline, SiteDesc};

    use crate::frame::px::Px;
    use crate::graph::Gx;

    const REL: u32 = 16_500;

    /// `(d int dropped, k text, w text)`, or with `byval` a live int after the texts.
    struct F {
        byval: bool,
    }

    impl Facts for F {
        fn rel(&self, relid: u32) -> Option<RelFact> {
            (relid == REL).then(|| RelFact::plain(REL, Am::Heap, b'r', if self.byval { 4 } else { 3 }))
        }
        fn att(&self, relid: u32, attno: i16) -> Option<AttFact> {
            let text = AttFact::plain(25, -1, -1, false, false);
            match (relid == REL, attno) {
                (true, 2 | 3) => Some(text),
                (true, 4) if self.byval => Some(AttFact::plain(23, -1, 4, true, false)),
                _ => None,
            }
        }
        fn index(&self, _: u32) -> Option<IndexFact> {
            None
        }
        fn indexes_of(&self, _: u32, _: &mut dyn FnMut(IndexFact)) {}
        fn index_expr(&self, _: u32, _: u8) -> Option<ExprFp> {
            None
        }
    }

    /// The driving attribute of an index scan reading no column over `F`, or the refusal.
    fn drive(byval: bool) -> Result<Vec<LaneId>, crate::Refusal> {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let mut g = PipelineGraph::new(a);
        g.pipelines.push(PipelineNode::new(a, 7, Source::Const, Pipeline::serial(-1, End::Sink, gravity_program::Reason::NoDrivingSite)));
        let facts = F { byval };
        let gx = Gx { graph: &g, facts: &facts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
        let mut plan = Sequential::new(a);
        let px = Px::new(a, &gx, &mut plan);
        let d = SiteDesc::index_source(a, REL, REL + 1, &[2], &[0], &[]);
        crate::catch(7, || super::index_site(&px, &d, REL, &[0], &[2], 7).0.lanes().iter().map(|l| l.id).collect())
    }

    /// D607: a relation with no by-value column drives by its first live column (the dropped one skipped), where it
    /// refused before; G4: a by-value column still drives when there is one.
    #[test]
    fn a_no_column_index_scan_without_a_by_value_column_drives_by_its_first_live_column() {
        assert_eq!(drive(false).map_err(|r| format!("{r:?}")), Ok(vec![LaneId::Attr(2)]));
        assert_eq!(drive(true).map_err(|r| format!("{r:?}")), Ok(vec![LaneId::Attr(4)]));
    }
}

//! Road `heap.pages` (H01 / H03, R1-L05 R2-L12; cards/M2.md L4, applies `heap`): the heap source's raw pinned pages walked
//! in IR (KEEP lowerv2 source/range.rs `pages_of` / `tuple_at` / `step_varlena` / `cells_of` / `next_window`'s page
//! protocol; v1 `lower_paged_heap`). The site is published raw (`SiteDesc::raw_pages`): per window one pinned page of the
//! claim, its visible line pointers (the offset lane, ascending) and its flags (`PAGE_FULL`: every visible tuple carries
//! the attributes the scan reads; `PAGE_NONULLS`: none has a null bitmap; `PAGE_UNREAD`: an all-visible page the host
//! answered from its line pointers, whose rows test their own header word, V4R-371). A row's cells are SSA datum words:
//! - a tuple on a full page without a null bitmap reads each column at its fixed offset from `t_hoff` up to the first
//!   varlena, then at a running offset (a varlena is stepped over by its header: the 1-byte-header road first, D062);
//! - a tuple with a null bitmap walks its present attributes (an absent one reads NULL);
//! - a page that is not full (a tuple with fewer attributes: missing defaults) is deformed by the host (`heap_page_lane`);
//!   on an unread page the first such row asks for the lanes.
//! A varlena cell is the datum pointer into the pinned page (valid until the window: a retain copies). The walk goes over
//! the line pointers last to first (ascending tuple addresses, Decision 11) under a plain or a hashed aggregate where the
//! effects home finds the row order invisible (`effects::visible::order_visible`: every aggregate order-free, D853, with
//! PG's min / max member, D862, and a hashed group's keys one image; no VOLATILE call, D831; no LIMIT / OFFSET, D1804, or
//! row lock, V4R-512; no outer-first hash probe after ops that may raise, D2593); forward everywhere else. A catalog
//! NOT NULL column has no validity test (P7). The window's facts are `nonulls` (full and no null bitmap, or unread),
//! `page_full` (or unread) and `nosel`: the fast body is the fixed-offset walk behind the header word's one test.
//! A by-reference column of fixed length (uuid, interval, name) sits at the running offset aligned to its `typalign` and is
//! stepped over by its `typlen`; read, its cell is the datum pointer into the pinned page, as a varlena's. A dropped column
//! ahead of a read one is stepped over by its kept layout (`Facts::dropped_layout`); a whole row reads it by that layout
//! (D2789). A site the walk cannot read (a type the catalog does not answer, a read lane in another form) keeps heap.rs's
//! host-deformed windows (`make` returns `None`).
//! Owner M2-HEAP.
use core::cell::{Cell as Once, RefCell};

use gravity_abi::ctx::{lane, site_off};
use gravity_abi::site::site;
use gravity_abi::BreakerKind;
use gravity_decompose::{Op, Source};
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{Block, ConvOp, HostFlags, HostFn, IntBinOp, IntCond, Type, Value};
use gravity_kernels::host_abi as abi;
use gravity_program::{Home, LaneId as SiteLane, Pipeline, Rep, SiteDesc, Until};
use mcx::{CollectIn, GrowIn, Mcx, PgBox, PgVec};

use crate::ops::scan::heap::{add_off, align_off, as_value, load_word, push_ctid, push_tableoid, set_rel, step_varlena, tid_image, Off, HEAP_HASNULL, LP_OFF_MASK, PAGE_HEADER, T_BITS, T_HOFF};
use crate::cell::{At, Cell, CellHome, CellLife, Nullable, Row};
use crate::face::source::{Locator, ReadPlan, SourceMech, WinConds};
use crate::frame::env::Env;
use crate::frame::frame::Win;
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::frame::site::CTID_AT;
use crate::admit::refusal::Reason;
use crate::ops::Table;

crate::strategies! {
    /// Raw pinned pages, fixed-offset deform, word driver, varlena pointers.
    HEAP_PAGES = 56, "heap.pages", Heap;
}

/// Read by heap.rs through `LaneForm::HeapPages.on()`: the road's mechanism is this file's `make`.
pub(crate) const ROWS: Table = Table { strategies: &[(HEAP_PAGES, &())], ..crate::ops::NONE };

/// One attribute on the walk: a by-value column of 1 / 2 / 4 / 8 bytes (aligned to its length), a by-reference column of
/// fixed length `len` aligned to its type's `typalign` (`att_align_nominal`), or a varlena of its `typalign`.
#[derive(Clone, Copy)]
enum Att {
    Fixed(u8),
    Ref { len: u16, align: u8 },
    Varlena(u8),
}

/// A column the rows read: its site lane, attribute, cell and whether the catalog declares it NOT NULL.
#[derive(Clone, Copy)]
struct PageCol {
    k: usize,
    attno: i16,
    notnull: bool,
    cell: Cell,
}

/// The cells one call makes: the walk's attributes `from..n` stepped over from offset `off`, the `cols` indices `idx`
/// (ascending) read; `tail`: its joins also carry the offset after attribute `n` (`row_early`'s, where `row_late` goes on).
#[derive(Clone, Copy)]
struct Part<'a> {
    n: usize,
    idx: &'a [usize],
    from: usize,
    off: Off,
    tail: bool,
}

/// The window's values the rows read (loaded once per window).
#[derive(Clone)]
struct PWin<'g> {
    page: Value,
    offs: Value,
    block: Value,
    /// The page's `page_flags` (the general body reads them per row: one word live across the fast body, V4R-371).
    flags: Value,
    sel: Value,
    sel_null: Value,
    /// The window's last row (`end - 1`): a reversed walk's first.
    last: Value,
    /// Per column `(lane ptr, validity words, no validity words)`: the host-deformed lanes of a page that is not full.
    lanes: PgVec<'g, (Value, Value, Value)>,
}

pub(crate) struct PagesMech<'g> {
    cols: PgVec<'g, PageCol>,
    walk: PgVec<'g, Att>,
    /// decode.late: the leading filter ops (0: every cell at the row's start), the walk's length they read and the `cols`
    /// indices of every column, of the columns they read, and of the rest.
    late_at: usize,
    early: usize,
    all: PgVec<'g, usize>,
    first: PgVec<'g, usize>,
    rest: PgVec<'g, usize>,
    mcx: Mcx<'g>,
    /// The body `row_early` lowered its row in (the fast body's header tests passed): `row_late` makes the rest there.
    early_fast: Once<bool>,
    early_off: Once<Off>,
    /// The last row's body (`row_of`), for `keep_row`.
    last_fast: Once<bool>,
    /// The row's column map size: the last attribute read, or `CTID_AT` when the rows read `ctid`.
    natts: usize,
    node: i32,
    off: usize,
    ctid: bool,
    /// D3681: a relation-set site (each row's relation its window's: heap.rs `set_rel`), and whether its rows carry their
    /// `tableoid` cell (heap.rs `push_tableoid`).
    set: bool,
    tableoid: bool,
    reverse: bool,
    hosts: Once<Option<[HostFn; 3]>>,
    win: RefCell<Option<PWin<'g>>>,
}

/// The alignment in bytes of a column of type `typid` the walk steps over: its storage alignment (`typalign`) from the
/// typesem home (`rep::storage_align`; the attribute facts carry none). A type the catalog does not answer keeps the host's
/// deform (SRC-31: no type list stands in for the catalog).
fn type_align(px: &Px<'_, '_>, typid: u32) -> Option<u8> {
    crate::rep::storage_align(px.gx.catalog, typid)
}

/// A dropped column on the walk: stepped over by the layout `pg_attribute` keeps (a tuple written before the drop carries
/// its bytes, a later one a NULL), and read only for a whole row (D2789). A by-value column aligned other than to its
/// length keeps the host's deform.
fn dropped_att(l: gravity_decompose::catalog::AttLayout) -> Option<Att> {
    match (l.byval, l.typlen) {
        (true, 1 | 2 | 4 | 8) if l.align as i16 == l.typlen => Some(Att::Fixed(l.typlen as u8)),
        (false, n) if n > 0 => Some(Att::Ref { len: n as u16, align: l.align }),
        (false, -1) => Some(Att::Varlena(l.align)),
        _ => None,
    }
}

/// The page form of heap site `s`, or `None` when the walk cannot reach a read column (v2 `pages_of`). Publishes the site
/// raw on success.
pub(crate) fn make<'g>(px: &mut Px<'g, '_>, k: usize, s: &SiteDesc, relid: u32, off: Option<usize>, ctid: bool) -> Option<PgBox<'g, dyn SourceMech + 'g>> {
    let off = off?;
    let mcx = px.mcx();
    let facts = px.gx.facts;
    let read: PgVec<'_, (usize, i16)> = s.lanes().iter().enumerate().filter_map(|(k, l)| if let SiteLane::Attr(a) = l.id { Some((k, a)) } else { None }).collect_in(mcx);
    let want = read.iter().map(|&(_, a)| a).max().unwrap_or(0);
    let mut walk = mcx::reserved_in(want as usize, mcx);
    let mut cols = mcx::reserved_in(read.len(), mcx);
    for a in 1..=want {
        let at = read.iter().find(|&&(_, x)| x == a).map(|&(k, _)| (k, s.lanes()[k]));
        let Some(f) = facts.att(relid, a) else {
            let att = dropped_att(facts.dropped_layout(relid, a)?)?;
            walk.push_in(att);
            // D2789: a dropped attribute a whole row reads (decompose `whole_row_atts`): its stored datum, a word or the datum
            // pointer into the pinned page by the kept layout; it has no type
            if let Some((lk, d)) = at {
                let cell = match att {
                    Att::Fixed(_) if d.kind != lane::KIND_BYREF => Cell { rep: Rep::Word { bytes: 8 }, typid: 0, num: d.num, life: CellLife { until: Until::Statement, home: CellHome::Lane(Home::Word) }, at: At::Value(a as u16), null: Nullable::Maybe },
                    Att::Ref { len, .. } if d.kind == lane::KIND_BYREF && d.rep == Rep::FixedPtr { len } => Cell { rep: d.rep, typid: 0, num: d.num, life: CellLife::of_lane(d.life), at: At::Value(a as u16), null: Nullable::Maybe },
                    Att::Varlena(_) if d.kind == lane::KIND_BYREF && d.rep == Rep::VarlenaPtr => Cell { rep: d.rep, typid: 0, num: d.num, life: CellLife::of_lane(d.life), at: At::Value(a as u16), null: Nullable::Maybe },
                    _ => return None,
                };
                cols.push_in(PageCol { k: lk, attno: a, notnull: false, cell });
            }
            continue;
        };
        let att = if f.byval && matches!(f.typlen, 1 | 2 | 4 | 8) {
            Att::Fixed(f.typlen as u8)
        } else if !f.byval && f.typlen > 0 && at.is_none_or(|(_, d)| d.kind == lane::KIND_BYREF && d.rep == Rep::FixedPtr { len: f.typlen as u16 }) {
            Att::Ref { len: f.typlen as u16, align: type_align(px, f.typid)? }
        } else if f.typlen == -1 && !f.byval && at.is_none_or(|(_, d)| d.kind == lane::KIND_BYREF && d.rep == Rep::VarlenaPtr) {
            Att::Varlena(type_align(px, f.typid)?)
        } else {
            return None;
        };
        walk.push_in(att);
        let Some((lk, d)) = at else { continue };
        let null = if f.notnull { Nullable::Never } else { Nullable::Maybe };
        // a fixed column's cell is its datum word (the host's by-value lane word, sign-extended from the type's length);
        // a by-reference one's (fixed length or varlena) is the datum pointer into the pinned page
        let cell = match att {
            Att::Fixed(_) => Cell { rep: Rep::Word { bytes: 8 }, typid: f.typid, num: d.num, life: CellLife { until: Until::Statement, home: CellHome::Lane(Home::Word) }, at: At::Value(a as u16), null },
            Att::Ref { .. } | Att::Varlena(_) => Cell { rep: d.rep, typid: f.typid, num: d.num, life: CellLife::of_lane(d.life), at: At::Value(a as u16), null },
        };
        cols.push_in(PageCol { k: lk, attno: a, notnull: f.notnull, cell });
    }
    let node = px.gx.pipelines()[k].node_id;
    // a plain or a hashed aggregate's end: a hashed group's records take its rows in any order the effects home finds
    // invisible, as a plain fold does (its keys print alike over equal members: `walk_free`), and the order its groups
    // leave the table in is the table's (nodeAgg.c agg_retrieve_hash_table). A sorted group on this end keeps the walk.
    // and every fold of the hashed group gives PostgreSQL's bits over its rows in any order (`fold::bits_free`: a float
    // fold rounds by the order PG's seq scan feeds the group, which D798's e2e compares bit for bit)
    let hashed = || px.gx.group_at(k).is_some_and(|g| g.form == gravity_decompose::ops::shared::agg::GroupForm::Hashed && crate::effects::fold::bits_free(g));
    let folds = px.gx.pipelines()[k].desc.breaker().is_some_and(|b| b.kind == BreakerKind::PlainAgg || b.kind == BreakerKind::HashAgg && hashed());
    let reverse = folds && crate::effects::visible::order_visible(px.gx, k).is_none();
    let natts = if ctid { CTID_AT as usize } else { want as usize };
    let (set, tableoid) = (s.set.len() > 1, crate::ops::scan::heap::set_tableoid(px, k, s));
    px.set_site(k, s.copy_in(px.arena).raw_pages());
    // the reversal also needs nests whose inners raise nothing (`effects::nests_raise_free`): a raising inner raises at the
    // first outer row the walk reaches, PostgreSQL's
    let reverse = reverse && crate::effects::nests_raise_free(px.gx, k);
    // D2662: and the walk's own raising code under a recheck licence, never a user function (asked last: it licenses `k`)
    let reverse = reverse && crate::effects::walk_reversible(px, k).is_some();
    let all: PgVec<'_, usize> = (0..cols.len()).collect_in(mcx);
    // a row that passes pays a re-entry (its tuple, the body's branches) for the late cells: worth it only when the walk
    // past the filters' columns steps over a varlena or a by-reference column; a fixed-width one is a few constant loads
    let late_walk = |e: usize| walk[e.min(walk.len())..].iter().any(|a| !matches!(a, Att::Fixed(_)));
    let (late_at, early) = late_split(px, k).filter(|&(_, e)| cols.iter().any(|c| c.attno as usize > e) && late_walk(e)).unwrap_or((0, walk.len()));
    let first = all.iter().copied().filter(|&x| cols[x].attno as usize <= early).collect_in(mcx);
    let rest = all.iter().copied().filter(|&x| cols[x].attno as usize > early).collect_in(mcx);
    Some(mcx::pg_box_dyn!(mcx, PagesMech { cols, walk, late_at, early, all, first, rest, mcx, early_fast: Once::new(false), early_off: Once::new(Off::K(0)), last_fast: Once::new(false), natts, node, off, ctid, set, tableoid, reverse, hosts: Once::new(None), win: RefCell::new(None) } => dyn SourceMech + 'g))
}

/// decode.late: pipeline `k`'s leading filter ops and the last attribute their quals read, when every column they read is
/// one of the scan's own attributes or its `ctid` (a whole-row or other system column, a column of another range table
/// entry or an expression the walk does not know keeps every cell eager: `None`).
fn late_split(px: &Px<'_, '_>, k: usize) -> Option<(usize, usize)> {
    let mcx = px.mcx();
    let p = &px.gx.pipelines()[k];
    let Source::Range { scanrelid, .. } = &p.source else { return None };
    let (mut n, mut last, mut other) = (0usize, 0usize, false);
    for op in p.ops.iter() {
        let Op::Filter(q) = op else { break };
        let quals = match q.as_list() {
            Some(l) => l.iter().collect_in(mcx),
            None => mcx::pg_vec![in mcx; *q],
        };
        for q in quals {
            let known = gravity_decompose::expr::walk(q, &mut |x| {
                if let Some(v) = x.as_var().filter(|v| v.varlevelsup == 0) {
                    match (v.varno == *scanrelid as i32, v.varattno) {
                        (true, a) if a > 0 => last = last.max(a as usize),
                        (true, -1) => {}
                        _ => other = true,
                    }
                }
                true
            });
            other |= !known;
        }
        n += 1;
    }
    (!other && n > 0).then_some((n, last))
}

/// The header word at 18 (V4R-371, 2-aligned): `t_infomask2` (natts, its low 11 bits) and `t_infomask` (HEAP_HASNULL at
/// bit 16).
const T_WORD: i32 = 18;
const NATTS_MASK: i64 = 0x7ff;

/// The tuple at visible index `i` (`offs[i]` → its line pointer, v2 `tuple_at`; `iota`, a `PAGE_IOTA` page: the index's
/// line pointer) and its `t_hoff`.
fn tuple_at(b: &mut FunctionBuilder<'_>, w: &PWin<'_>, i: Value, iota: bool) -> (Value, Value) {
    let lpp = if iota {
        b.ptr_add(w.page, i, 4, PAGE_HEADER)
    } else {
        let op = b.ptr_add(w.offs, i, 2, 0);
        let off = b.load_zext(Type::I16, op, 0);
        b.ptr_add(w.page, off, 4, PAGE_HEADER - 4)
    };
    let lp = b.load_zext(Type::I32, lpp, 0);
    let m = b.iconst(Type::I64, LP_OFF_MASK);
    let lp_off = b.int_bin(IntBinOp::And, lp, m);
    let tup = b.ptr_add(w.page, lp_off, 1, 0);
    (tup, b.load_zext(Type::I8, tup, T_HOFF))
}

/// `tuple_at` and the tuple's header word.
fn tuple_word(b: &mut FunctionBuilder<'_>, w: &PWin<'_>, i: Value, iota: bool) -> (Value, Value, Value) {
    let (tup, hoff) = tuple_at(b, w, i, iota);
    (tup, b.load_zext_unaligned(Type::I32, tup, T_WORD), hoff)
}

/// `page_flags & m` and the mask.
fn page_flag(b: &mut FunctionBuilder<'_>, flags: Value, m: u16) -> (Value, Value) {
    let mv = b.iconst(Type::I64, m as i64);
    (b.int_bin(IntBinOp::And, flags, mv), mv)
}

/// The header word's tests, each where its branch reads it: a null bitmap (`true`), else fewer attributes than `want`.
fn word_test(b: &mut FunctionBuilder<'_>, env: &Env, word: Value, want: Option<usize>) -> Value {
    let (m, cond, rhs) = match want {
        None => (HEAP_HASNULL << 16, IntCond::Ne, env.zero),
        Some(n) => (NATTS_MASK, IntCond::Ult, b.iconst(Type::I64, n as i64)),
    };
    let m = b.iconst(Type::I64, m);
    let x = b.int_bin(IntBinOp::And, word, m);
    b.icmp(cond, x, rhs)
}

impl<'g> PagesMech<'g> {
    fn hosts(&self) -> [HostFn; 3] {
        match self.hosts.get() {
            Some(h) => h,
            None => crate::refuse!(self.node, Reason::Bug { payload: "a heap page source lowered before its declare" }),
        }
    }

    fn vals(&self) -> PWin<'g> {
        match self.win.borrow().clone() {
            Some(w) => w,
            None => crate::refuse!(self.node, Reason::Bug { payload: "a heap page row before its window" }),
        }
    }

    fn k(&self, env: &Env) -> usize {
        match env.site {
            Some(k) => k,
            None => crate::refuse!(self.node, Reason::Bug { payload: "a heap page source without its site" }),
        }
    }

    /// The read columns of a tuple without a null bitmap along the walk from `data`, in `cols` order. `short`: the
    /// 1-byte-header road (D062) and its verdict (no varlena read length 0: the external tag 0x01). The road speculates on
    /// the short header: an even header byte branches to `short`'s block (the full walk), so the running offset is one load
    /// and one add per varlena (the mask form's and / neg / and sat on that chain: 8 -> 6 cycles per varlena on V2).
    fn fixed(&self, b: &mut FunctionBuilder<'_>, data: Value, short: Option<Block>, p: Part<'_>) -> (PgVec<'g, Value>, Option<Value>, Off) {
        let mut off = p.off;
        let mut least: Option<Value> = None;
        let mut out = mcx::reserved_in(p.idx.len(), self.mcx);
        for (a, &att) in self.walk.iter().enumerate().take(p.n).skip(p.from) {
            let read = self.reads(p, a);
            match (att, short) {
                (Att::Fixed(len), _) => {
                    off = align_off(b, off, len as i64);
                    if read {
                        let o = as_value(b, off);
                        let p = b.ptr_add(data, o, 1, 0);
                        out.push_in(load_word(b, p, len));
                    }
                    off = add_off(b, off, len as i64);
                }
                (Att::Ref { len, align }, _) => {
                    off = align_off(b, off, align as i64);
                    if read {
                        let o = as_value(b, off);
                        let p = b.ptr_add(data, o, 1, 0);
                        out.push_in(b.conv(ConvOp::PtrToInt, Type::I64, p));
                    }
                    off = add_off(b, off, len as i64);
                }
                (Att::Varlena(_), Some(l_full)) => {
                    let o = as_value(b, off);
                    let p = b.ptr_add(data, o, 1, 0);
                    let hb = b.load_zext(Type::I8, p, 0);
                    let one = b.iconst(Type::I64, 1);
                    let odd = b.int_bin(IntBinOp::And, hb, one);
                    let zero = b.iconst(Type::I64, 0);
                    let is_short = b.icmp(IntCond::Ne, odd, zero);
                    let l_next = b.create_block(&[]);
                    b.cond_br(is_short, (l_next, &[]), (l_full, &[]));
                    b.switch_to(l_next);
                    let len = b.int_bin(IntBinOp::Ushr, hb, one);
                    least = Some(least.map_or(len, |m| b.int_bin(IntBinOp::Umin, m, len)));
                    if read {
                        out.push_in(b.conv(ConvOp::PtrToInt, Type::I64, p));
                    }
                    off = Off::V(b.int_bin(IntBinOp::Add, o, len));
                }
                (Att::Varlena(align), None) => {
                    let (after, p) = step_varlena(b, data, off, align, None);
                    if read {
                        out.push_in(b.conv(ConvOp::PtrToInt, Type::I64, p));
                    }
                    off = Off::V(after);
                }
            }
        }
        let ok = least.map(|m| {
            let zero = b.iconst(Type::I64, 0);
            b.icmp(IntCond::Ne, m, zero)
        });
        (out, ok, off)
    }

    /// `fixed` with the short road first when a varlena is on the walk; any other header form takes the full walk (cold).
    fn fixed_cells(&self, b: &mut FunctionBuilder<'_>, data: Value, p: Part<'_>) -> (PgVec<'g, Value>, Off) {
        if !self.walk[p.from.min(p.n)..p.n].iter().any(|a| matches!(a, Att::Varlena(_))) {
            let (out, _, off) = self.fixed(b, data, None, p);
            return (out, off);
        }
        let tys = mcx::pg_vec![in self.mcx; Type::I64; p.idx.len() + p.tail as usize];
        let (l_join, l_full) = (b.create_block(&tys[..]), b.create_block(&[]));
        b.mark_cold(l_full);
        let (mut short, ok, so) = self.fixed(b, data, Some(l_full), p);
        if p.tail {
            short.push_in(as_value(b, so));
        }
        let ok = ok.unwrap_or_else(|| b.bconst(true));
        b.cond_br(ok, (l_join, &short[..]), (l_full, &[]));
        b.switch_to(l_full);
        let (mut full, _, fo) = self.fixed(b, data, None, p);
        if p.tail {
            full.push_in(as_value(b, fo));
        }
        b.br(l_join, &full[..]);
        b.switch_to(l_join);
        let mut out = mcx::to_vec_in(&b.block_params(l_join)[..], self.mcx);
        let end = if p.tail { out.pop().map_or(Off::K(0), Off::V) } else { Off::K(0) };
        (out, end)
    }

    /// A tuple with a null bitmap: attribute a is present when bit a - 1 of `t_bits` is set; a present attribute sits at
    /// the running offset aligned to its length and advances it; an absent one is NULL (v2 `deform`'s null walk). Returns
    /// `(word, null)` per read column.
    fn null_walk(&self, b: &mut FunctionBuilder<'_>, env: &Env, tup: Value, hoff: Value, p: Part<'_>) -> (PgVec<'g, (Value, Value)>, Value) {
        let data = b.ptr_add(tup, hoff, 1, 0);
        let mut off = match p.off {
            Off::K(0) => env.zero,
            o => as_value(b, o),
        };
        let mut byte = (usize::MAX, env.zero);
        let mut out = mcx::reserved_in(p.idx.len(), self.mcx);
        for (a, &att) in self.walk.iter().enumerate().take(p.n).skip(p.from) {
            if byte.0 != a / 8 {
                byte = (a / 8, b.load_zext(Type::I8, tup, T_BITS + (a / 8) as i32));
            }
            let sh = b.iconst(Type::I64, (a % 8) as i64);
            let bit = b.int_bin(IntBinOp::Ushr, byte.1, sh);
            let bit1 = b.int_bin(IntBinOp::And, bit, env.one);
            let present = b.icmp(IntCond::Ne, bit1, env.zero);
            let absent = b.icmp(IntCond::Eq, bit1, env.zero);
            let read = self.reads(p, a);
            let after = match att {
                Att::Fixed(len) => {
                    let aligned = align_off(b, Off::V(off), len as i64);
                    let aligned = as_value(b, aligned);
                    if read {
                        let rel = b.select(present, aligned, env.zero);
                        let base = b.select(present, data, tup);
                        let p = b.ptr_add(base, rel, 1, 0);
                        out.push_in((load_word(b, p, len), absent));
                    }
                    let l = b.iconst(Type::I64, len as i64);
                    b.int_bin(IntBinOp::Add, aligned, l)
                }
                Att::Ref { len, align } => {
                    let aligned = align_off(b, Off::V(off), align as i64);
                    let aligned = as_value(b, aligned);
                    if read {
                        // an absent one's pointer is the tuple header's (never read: the cell is NULL)
                        let rel = b.select(present, aligned, env.zero);
                        let base = b.select(present, data, tup);
                        let p = b.ptr_add(base, rel, 1, 0);
                        out.push_in((b.conv(ConvOp::PtrToInt, Type::I64, p), absent));
                    }
                    let l = b.iconst(Type::I64, len as i64);
                    b.int_bin(IntBinOp::Add, aligned, l)
                }
                Att::Varlena(align) => {
                    let (after, p) = step_varlena(b, data, Off::V(off), align, Some((present, tup)));
                    if read {
                        out.push_in((b.conv(ConvOp::PtrToInt, Type::I64, p), absent));
                    }
                    after
                }
            };
            off = b.select(present, after, off);
        }
        (out, off)
    }

    /// Row `j`'s visible index: reversed under a plain or a hashed aggregate, then through the window's selection unless
    /// the body has none (a raw page never selects; an EvalPlanQual window is the host's filled lanes).
    fn index(&self, b: &mut FunctionBuilder<'_>, env: &Env, w: &PWin<'_>, j: Value) -> Value {
        let jj = if self.reverse {
            b.int_bin(IntBinOp::Sub, w.last, j)
        } else {
            j
        };
        if env.body.nosel {
            return jj;
        }
        let (l_gather, l_row) = (b.create_block(&[]), b.create_block(&[Type::I64]));
        b.cond_br(w.sel_null, (l_row, &[jj]), (l_gather, &[]));
        b.switch_to(l_gather);
        let ip = b.ptr_add(w.sel, jj, 4, 0);
        let i = b.load_zext(Type::I32, ip, 0);
        b.br(l_row, &[i]);
        b.switch_to(l_row);
        b.block_params(l_row)[0]
    }

    /// The general body's cells: a full or unread page's tuple (`tuple_cells`), else the host's lanes.
    fn general(&self, b: &mut FunctionBuilder<'_>, env: &Env, w: &PWin<'_>, i: Value, p: Part<'_>) -> (PgVec<'g, Value>, Option<Value>) {
        let (l_done, l_tup, l_lane) = (self.pair_block(b, p), b.create_block(&[]), b.create_block(&[]));
        if !self.walk.is_empty() {
            // a page the host walked with both flags known (a gap in its line pointers: no PAGE_IOTA) reads its fixed offsets
            let (l_known, l_rest) = (b.create_block(&[]), b.create_block(&[]));
            let (bw, both) = page_flag(b, w.flags, site::PAGE_FULL | site::PAGE_NONULLS);
            let known = b.icmp(IntCond::Eq, bw, both);
            b.cond_br(known, (l_known, &[]), (l_rest, &[]));
            b.switch_to(l_known);
            let (tup, hoff) = tuple_at(b, w, i, false);
            let (mut args, off) = self.fixed_flat(b, tup, hoff, p);
            if p.tail {
                args.push_in(as_value(b, off));
            }
            b.br(l_done, &args[..]);
            b.switch_to(l_rest);
        }
        let (fw, _) = page_flag(b, w.flags, site::PAGE_FULL | site::PAGE_UNREAD);
        let full = b.icmp(IntCond::Ne, fw, env.zero);
        b.cond_br(full, (l_tup, &[]), (l_lane, &[]));
        b.switch_to(l_tup);
        let (tup, word, hoff) = tuple_word(b, w, i, false);
        let (mut args, off) = self.tuple_cells(b, env, i, [tup, word, hoff], p);
        args.extend_in(off);
        b.br(l_done, &args[..]);
        b.switch_to(l_lane);
        let mut args = self.lane_cells(b, env, i, &w.lanes[..], p);
        if p.tail {
            args.push_in(env.zero);
        }
        b.br(l_done, &args[..]);
        b.switch_to(l_done);
        let mut out = mcx::to_vec_in(&b.block_params(l_done)[..], self.mcx);
        let end = if p.tail { out.pop() } else { None };
        (out, end)
    }

    /// The fast body's row (V4R-371): the header word's tests, passed by every row of a page the host walked
    /// (`PAGE_NONULLS | PAGE_FULL`) and asked here of an unread page's (no null bitmap, at least the walk's attributes), then
    /// the fixed offsets. A row that fails re-enters the window in the general body at its own row (`Env::escape`, cold).
    fn checked(&self, b: &mut FunctionBuilder<'_>, env: &Env, w: &PWin<'_>, i: Value, l_row: Block, (j, carry): (Value, &Carry), p: Part<'_>) -> (PgVec<'g, Value>, Off) {
        let (l_long, l_other, l_fixed) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        b.mark_cold(l_other);
        let (tup, word, hoff) = tuple_word(b, w, i, true);
        let hasnull = word_test(b, env, word, None);
        b.cond_br(hasnull, (l_other, &[]), (l_long, &[]));
        b.switch_to(l_long);
        let short = word_test(b, env, word, Some(self.walk.len()));
        b.cond_br(short, (l_other, &[]), (l_fixed, &[]));
        b.switch_to(l_other);
        b.br(l_row, &mcx::concat_in(&[&[j][..], &carry.params()[..]], self.mcx)[..]);
        b.switch_to(l_fixed);
        self.fixed_flat(b, tup, hoff, p)
    }

    /// A tuple without a null bitmap: its fixed-offset cells, flat `(word, false)` pairs.
    fn fixed_flat(&self, b: &mut FunctionBuilder<'_>, tup: Value, hoff: Value, p: Part<'_>) -> (PgVec<'g, Value>, Off) {
        let (data, f) = (b.ptr_add(tup, hoff, 1, 0), b.bconst(false));
        let (cells, off) = self.fixed_cells(b, data, p);
        (cells.into_iter().flat_map(|v| [v, f]).collect_in(self.mcx), off)
    }

    /// A tuple's flat `(word, null)` cells by its header word `[tup, word, hoff]`: fewer attributes than the walk (an unread
    /// page's: `short_cells`, cold), a null bitmap (the null walk), else the fixed offsets.
    fn tuple_cells(&self, b: &mut FunctionBuilder<'_>, env: &Env, i: Value, [tup, word, hoff]: [Value; 3], p: Part<'_>) -> (PgVec<'g, Value>, Option<Value>) {
        let (l_done, l_short, l_long, l_fixed, l_walk) = (self.pair_block(b, p), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        b.mark_cold(l_short);
        let short = word_test(b, env, word, Some(self.walk.len()));
        b.cond_br(short, (l_short, &[]), (l_long, &[]));
        b.switch_to(l_long);
        let hasnull = word_test(b, env, word, None);
        b.cond_br(hasnull, (l_walk, &[]), (l_fixed, &[]));
        b.switch_to(l_fixed);
        let (mut args, off) = self.fixed_flat(b, tup, hoff, p);
        if p.tail {
            args.push_in(as_value(b, off));
        }
        b.br(l_done, &args[..]);
        b.switch_to(l_walk);
        let (pairs, off) = self.null_walk(b, env, tup, hoff, p);
        let mut args = pairs.into_iter().flat_map(|(v, nl)| [v, nl]).collect_in(self.mcx);
        if p.tail {
            args.push_in(off);
        }
        b.br(l_done, &args[..]);
        b.switch_to(l_short);
        let mut args = self.short_cells(b, env, i, p);
        if p.tail {
            args.push_in(env.zero);
        }
        b.br(l_done, &args[..]);
        b.switch_to(l_done);
        let mut out = mcx::to_vec_in(&b.block_params(l_done)[..], self.mcx);
        let end = if p.tail { out.pop() } else { None };
        (out, end)
    }

    /// An unread page's short tuple (missing attributes): each read column's lane asked for once per window
    /// (`heap_page_lane` deforms the page's rows; a failing status leaves), then read as a not-full page's.
    fn short_cells(&self, b: &mut FunctionBuilder<'_>, env: &Env, i: Value, p: Part<'_>) -> PgVec<'g, Value> {
        let (fill, k) = (self.hosts()[2], self.k(env));
        let kv = b.iconst(Type::I64, k as i64);
        let arr = b.load(Type::Ptr, env.ctx, site_off(k) as i32 + site::LANES as i32);
        for c in p.idx.iter().map(|&x| self.cols[x]) {
            let ptr = b.load(Type::Ptr, arr, (c.k * lane::SIZE + lane::PTR) as i32);
            let pi = b.conv(ConvOp::PtrToInt, Type::I64, ptr);
            let none = b.icmp(IntCond::Eq, pi, env.zero);
            let (l_fill, l_ok) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(none, (l_fill, &[]), (l_ok, &[]));
            b.switch_to(l_fill);
            let lk = b.iconst(Type::I64, c.k as i64);
            let rc = b.call_host(fill, &[env.ctx, kv, lk], Some(Type::I64)).unwrap_or(env.zero);
            let bad = b.icmp(IntCond::Slt, rc, env.zero);
            b.cond_br(bad, (env.x.l_exit, &[rc]), (l_ok, &[]));
            b.switch_to(l_ok);
        }
        let lanes = self.lane_descs(b, env, arr);
        self.lane_cells(b, env, i, &lanes[..], p)
    }

    /// Per read column its lane's `(values, validity words, no validity words)` from the site's lane array `arr`.
    fn lane_descs(&self, b: &mut FunctionBuilder<'_>, env: &Env, arr: Value) -> PgVec<'g, (Value, Value, Value)> {
        let mut lanes = mcx::reserved_in(self.cols.len(), self.mcx);
        for c in self.cols.iter() {
            let at = (c.k * lane::SIZE) as i32;
            let ptr = b.load(Type::Ptr, arr, at + lane::PTR as i32);
            let valid = b.load(Type::Ptr, arr, at + lane::VALID as i32);
            let vi = b.conv(ConvOp::PtrToInt, Type::I64, valid);
            lanes.push_in((ptr, valid, b.icmp(IntCond::Eq, vi, env.zero)));
        }
        lanes
    }

    /// Row `i`'s flat `(word, null)` cells from the host's lanes.
    fn lane_cells(&self, b: &mut FunctionBuilder<'_>, env: &Env, i: Value, lanes: &[(Value, Value, Value)], p: Part<'_>) -> PgVec<'g, Value> {
        let mut args = mcx::reserved_in(2 * p.idx.len(), self.mcx);
        for (c, &(ptr, valid, vnull)) in p.idx.iter().map(|&x| (&self.cols[x], &lanes[x])) {
            let p = b.ptr_add(ptr, i, 8, 0);
            let v = b.load(Type::I64, p, 0);
            args.extend_in([v, null_bit(b, env, i, valid, vnull, c.notnull)]);
        }
        args
    }

    /// A join block taking a `(word, null)` pair per read column.
    fn pair_block(&self, b: &mut FunctionBuilder<'_>, p: Part<'_>) -> Block {
        let mut tys = (0..p.idx.len()).flat_map(|_| [Type::I64, Type::Bool]).collect_in(self.mcx);
        if p.tail {
            tys.push_in(Type::I64);
        }
        b.create_block(&tys[..])
    }

    /// Whether `p` reads attribute `a + 1`.
    fn reads(&self, p: Part<'_>, a: usize) -> bool {
        p.idx.iter().any(|&x| self.cols[x].attno as usize == a + 1)
    }

    /// Every cell (`row`), the cells the leading filters read (`row_early`) or the rest (`row_late`).
    fn part(&self, which: Option<bool>) -> Part<'_> {
        match which {
            None => Part { n: self.walk.len(), idx: &self.all[..], from: 0, off: Off::K(0), tail: false },
            Some(true) => Part { n: self.early, idx: &self.first[..], from: 0, off: Off::K(0), tail: true },
            Some(false) => Part { n: self.walk.len(), idx: &self.rest[..], from: self.early, off: self.early_off.get(), tail: false },
        }
    }

    /// Row `j`: every cell (`which` none), or the leading filters' cells (`Some(true)`) with placeholders for the rest.
    fn row_of<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, j: Value, carry: Carry<'e>, next: Value, which: Option<bool>) -> RowVals<'e> {
        let w = self.vals();
        let i = self.index(b, env, &w, j);
        let p = self.part(which);
        // the fast body re-enters the general one at a failing row (frame.rs sets the escape wherever it lowers one)
        let fast = env.escape.filter(|_| env.body.nonulls && env.body.page_full);
        if which.is_some() {
            self.early_fast.set(fast.is_some());
        }
        self.last_fast.set(fast.is_some());
        let (cells, end) = match fast {
            None if which.is_some() && p.idx.is_empty() => (PgVec::new_in(self.mcx), Off::K(0)),
            None => {
                let (cells, end) = self.general(b, env, &w, i, p);
                (cells, end.map_or(Off::K(0), Off::V))
            }
            Some(_) if self.walk.is_empty() => (PgVec::new_in(self.mcx), Off::K(0)), // a count-only walk reads no tuple
            Some(l_row) => self.checked(b, env, &w, i, l_row, (j, &carry), p),
        };
        if which.is_some() {
            self.early_off.set(end);
        }
        let mut vals = mcx::pg_vec![in env.mcx; (env.zero, None); self.cols.len()];
        if p.idx.len() < self.cols.len() {
            let hole = b.bconst(true);
            for &x in self.rest.iter().filter(|&&x| !self.cols[x].notnull) {
                vals[x].1 = Some(hole);
            }
        }
        for (pair, &x) in cells.chunks(2).zip(p.idx.iter()) {
            vals[x] = (pair[0], (!self.cols[x].notnull).then_some(pair[1]));
        }
        let mut cols = mcx::pg_vec![in env.mcx; None; self.natts];
        for (n, c) in self.cols.iter().enumerate() {
            cols[c.attno as usize - 1] = Some(n as u16);
        }
        let cells = self.cols.iter().map(|c| c.cell).collect_in(env.mcx);
        let mut row = RowVals { row: Row { cells }, vals, cols, carry, next, at: j };
        if self.ctid {
            let off = self.tid(b, &w, i);
            let img = tid_image(b, w.block, off);
            push_ctid(&mut row, img);
        }
        if self.tableoid {
            let rel = set_rel(b, env, self.k(env));
            push_tableoid(b, &mut row, rel);
        }
        row
    }

    fn tid(&self, b: &mut FunctionBuilder<'_>, w: &PWin<'_>, i: Value) -> Value {
        let p = b.ptr_add(w.offs, i, 2, 0);
        b.load(Type::I16, p, 0)
    }
}

/// Row `i`'s NULL flag from a host lane's validity words (none: all valid); a NOT NULL column is never NULL.
fn null_bit(b: &mut FunctionBuilder<'_>, env: &Env, i: Value, valid: Value, vnull: Value, notnull: bool) -> Value {
    if notnull {
        return b.bconst(false);
    }
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

impl<'g> SourceMech for PagesMech<'g> {
    /// D824: a lazy build's probe sees the rows in PG's order (the build runs at the first).
    fn in_order(&mut self) {
        self.reverse = false;
    }

    fn declare(&mut self, pb: &mut ProgramBuilder) {
        let mem = HostFlags { touches_memory: true, ..HostFlags::default() };
        let seek = pb.declare_host("range_seek", &[Type::Ptr, Type::I64], Some(Type::I64), gravity_kernels::contract::entry1::<abi::RangeSeek> as *const () as usize, mem);
        let advance = pb.declare_host("advance", &[Type::Ptr, Type::I64], Some(Type::I64), gravity_kernels::contract::entry1::<abi::Advance> as *const () as usize, mem);
        let lane = pb.declare_host("heap_page_lane", &[Type::Ptr, Type::I64, Type::I64], Some(Type::I64), gravity_kernels::ops::scan::entries::heap_page_lane as *const () as usize, mem);
        self.hosts.set(Some([seek, advance, lane]));
    }

    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize) -> Pipeline {
        crate::ops::scan::heap::pipeline_desc(px, k)
    }

    fn read_plan(&self) -> ReadPlan {
        crate::ops::scan::heap::read_plan()
    }

    fn fills(&self) -> bool {
        false
    }

    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        let k = b.iconst(Type::I64, self.k(env) as i64);
        b.call_host(self.hosts()[0], &[env.ctx, k], Some(Type::I64)).unwrap_or(env.zero)
    }

    /// The next page (`advance`: a stop resumes inside the current one), its flags and offsets; a page that is not full
    /// has its read columns deformed by the host before the rows (`heap_page_lane`, a failing status leaves).
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let k = self.k(env);
        let [_, advance, fill] = self.hosts();
        let kv = b.iconst(Type::I64, k as i64);
        let r = b.call_host(advance, &[env.ctx, kv], Some(Type::I64)).unwrap_or(env.zero);
        let le = b.icmp(IntCond::Sle, r, env.zero);
        let l_window = b.create_block(&[]);
        b.cond_br(le, (l_eof, &[r]), (l_window, &[]));
        b.switch_to(l_window);
        let s = site_off(k) as i32;
        let cursor = b.load_zext(Type::I32, env.ctx, s + site::CURSOR as i32);
        let rows = b.load_zext(Type::I32, env.ctx, s + site::ROWS as i32);
        let sel = b.load(Type::Ptr, env.ctx, s + site::SEL as i32);
        let si = b.conv(ConvOp::PtrToInt, Type::I64, sel);
        let sel_null = b.icmp(IntCond::Eq, si, env.zero);
        let sel_n = b.load_zext(Type::I32, env.ctx, s + site::SEL_N as i32);
        let end = b.select(sel_null, rows, sel_n);
        let page = b.load(Type::Ptr, env.ctx, s + site::PAGE as i32);
        let flags = b.load_zext(Type::I16, env.ctx, s + site::PAGE_FLAGS as i32);
        // V4R-371: an unread page counts as full and null-free (its rows test their own header word); the fast body's
        // page is also PAGE_IOTA
        let flag = |b: &mut FunctionBuilder<'_>, m: u16| page_flag(b, flags, m);
        let ((uw, _), (fw, _), (bw, both), (iw, _)) = (flag(b, site::PAGE_UNREAD), flag(b, site::PAGE_FULL | site::PAGE_UNREAD), flag(b, site::PAGE_FULL | site::PAGE_NONULLS), flag(b, site::PAGE_IOTA));
        let (unread, full, known, iota) = (b.icmp(IntCond::Ne, uw, env.zero), b.icmp(IntCond::Ne, fw, env.zero), b.icmp(IntCond::Eq, bw, both), b.icmp(IntCond::Ne, iw, env.zero));
        let nonulls = b.int_bin(IntBinOp::Or, known, unread);
        let fast = b.int_bin(IntBinOp::And, full, iota);
        if !self.cols.is_empty() {
            let l_ready = b.create_block(&[]);
            let l_fill = b.create_block(&[]);
            b.cond_br(full, (l_ready, &[]), (l_fill, &[]));
            b.switch_to(l_fill);
            for c in self.cols.iter() {
                let lk = b.iconst(Type::I64, c.k as i64);
                let rc = b.call_host(fill, &[env.ctx, kv, lk], Some(Type::I64)).unwrap_or(env.zero);
                let bad = b.icmp(IntCond::Slt, rc, env.zero);
                let l_ok = b.create_block(&[]);
                b.cond_br(bad, (l_eof, &[rc]), (l_ok, &[]));
                b.switch_to(l_ok);
            }
            b.br(l_ready, &[]);
            b.switch_to(l_ready);
        }
        let arr = b.load(Type::Ptr, env.ctx, s + site::LANES as i32);
        let offs = b.load(Type::Ptr, arr, (self.off * lane::SIZE + lane::PTR) as i32);
        let block = b.load(Type::I32, env.ctx, s + site::POS_BLOCK as i32);
        let lanes = self.lane_descs(b, env, arr);
        let last = b.int_bin(IntBinOp::Sub, end, env.one);
        *self.win.borrow_mut() = Some(PWin { page, offs, block, flags, sel, sel_null, last, lanes });
        Win { cursor, end, facts: WinConds { nonulls: Some(nonulls), nosel: Some(sel_null), page_full: Some(fast), ..WinConds::default() } }
    }

    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        self.row_of(b, env, j, carry, next, None)
    }

    fn late_at(&self) -> usize {
        self.late_at
    }

    fn row_early<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        self.row_of(b, env, j, carry, next, Some(true))
    }

    /// The rest of the row's cells where it passed the leading filters, in the body `row_early` chose: the walk goes on
    /// from the offset `row_early`'s stopped at (the general body's branches taken again over the same tuple), the fast
    /// body's header tests already passed.
    fn row_late<'x, 'e: 'x>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, mut row: RowVals<'x>) -> RowVals<'x> {
        let w = self.vals();
        let i = self.index(b, env, &w, row.at);
        let p = self.part(Some(false));
        let cells = match self.early_fast.get() {
            false => self.general(b, env, &w, i, p).0,
            true => {
                let (tup, hoff) = tuple_at(b, &w, i, true);
                self.fixed_flat(b, tup, hoff, p).0
            }
        };
        for (pair, &x) in cells.chunks(2).zip(p.idx.iter()) {
            row.vals[x] = (pair[0], (!self.cols[x].notnull).then_some(pair[1]));
        }
        row
    }

    /// Road `mask.rows`: a fast body's row (its header tests passed: inline cells, pointers into the pinned page) is kept;
    /// `row_late` resumes from `row_early`'s end offset.
    fn keep_row(&self, early: bool) -> Option<Option<Value>> {
        match early {
            true => self.early_fast.get().then(|| match self.early_off.get() {
                Off::V(v) => Some(v),
                Off::K(_) => None,
            }),
            false => self.last_fast.get().then_some(None),
        }
    }

    fn resume_late(&self, at: Value) {
        self.early_off.set(Off::V(at));
    }

    /// H28: the row's `(block, offset)`; the pinned buffer is not published (`pinned = None`). D3681: a relation set's
    /// row's own relation (heap.rs `set_rel`).
    fn locator(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals) -> Option<Locator> {
        let w = self.vals();
        let i = self.index(b, env, &w, row.at);
        let off = self.tid(b, &w, i);
        let rel = self.set.then(|| set_rel(b, env, self.k(env)));
        Some(Locator { block: b.conv(ConvOp::Zext, Type::I64, w.block), offset: b.conv(ConvOp::Zext, Type::I64, off), pinned: None, rel })
    }
}

//! Road `topk.fetch` (cards/M3.md L5, V4R-169; v2 `source/emit_fetch.rs`, a12db31bd7c): a wide top over a
//! pgrc or heap range (props/topk.rs `topk_wide`) keeps `(key, locator)` rows in its run, the key columns by value and the
//! scan's locator word (`locator_word`: pgrc's `part << 32 | granule << 13 | row`, a heap row's TID word; hosts
//! ops/sort/heap_fetch.rs fetches a heap's winners by TID under the scan's snapshot). The emit reads the ordered run and fetches its rows'
//! columns once, a window of at most `MAX_FETCH_ROWS` rows a request, through the by-rowid fetch site (hosts `fetch_site`,
//! exec v4 m3_fetch.rs): the site's request lane pointed at the window's locators, one `Site.fetch` (CAP-25). The parts a locator names stay held for the statement: the fetch source
//! resolves them through the statement's part list (`Resolved`, the scan's snapshot and its part pins), so no compaction or
//! delete after the pass moves or drops the row. A top with the road off keeps every column by value. Owner M3-TOPS.
use core::cell::Cell as Once;

use gravity_abi::ctx::{lane, site_off};
use gravity_abi::site::{site, MAX_FETCH_ROWS};
use gravity_decompose::catalog::typefacts::{row_of, TypeFacts};
use gravity_decompose::graph::Bound;
use gravity_decompose::{Am, Op, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, IntBinOp, IntCond, Type, Value};
use gravity_program::{LaneDesc, NumTypmod, Rep, SiteDesc, Until};
use mcx::{CollectIn, GrowIn, Mcx, PgVec};

use crate::cell::{At, Cell, CellHome, CellLife, Nullable, Row};
use crate::face::source::{Locator, SourceMech};
use crate::frame::env::Env;
use crate::frame::frame::Win;
use crate::face::kinds::Face;
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{runs_word, Px, WordKey};
use crate::frame::row::{Carry, RowVals};
use crate::admit::refusal::Reason;
use crate::ops::Table;

use crate::rep::keylane::Key;
use crate::ops::sort::strategies::resident as res;
use crate::ops::sort::lower::{sort_of, RunEmit};
use crate::face::runs::{Col, Form, Shape};

crate::strategies! {
    /// Fetch form: (image, locator), fetch winners once.
    TOPK_FETCH = 67, "topk.fetch", TopkWide;
}

pub(crate) const ROWS: Table = Table { strategies: &[(TOPK_FETCH, &())], ..crate::ops::NONE };

/// `INT8OID` (the locator column).
const INT8: u32 = 20;

fn bug(node: i32, payload: &'static str) -> ! {
    crate::refuse!(node, Reason::Bug { payload })
}

/// Pipeline `k`'s fetch form when its top takes it: the road on and `topk_wide` (every sorted entry a bare scan column,
/// every key a by-value lane): the relation, the entries' attnos in entry order, and whether it is a heap (`locator_word`).
/// A member of an owner's rerun set keeps full rows (C-R6f: only the run reader re-opens per run, topk_rows.rs `D_TOP_RERUN`).
pub(crate) fn of<'g>(px: &Px<'g, '_>, k: usize) -> Option<(u32, PgVec<'g, i16>, bool)> {
    let p = &px.gx.pipelines()[k];
    let (sd, cols) = sort_of(px, k);
    // a union arm's locator is its own leaf's: the emit fetches through one relation, so a union's arms keep full rows;
    // a LATERAL top run per outer row (a rerun set member) keeps them too, read by RunEmit, which re-opens per run
    if !res::on(px, TOPK_FETCH) || !crate::props::topk_wide(k, p, sd, Some(cols), px.mcx()) || crate::ops::sort::lower::union_arm(p).is_some() || crate::rerun::sets::in_owner_set(px.gx, k) {
        return None;
    }
    // D3585: a relation set's TIDs are each relation's own: its top keeps full rows; so does a rows-road site (G4)
    let Source::Range { relid, am, ref site, .. } = p.source else { return None };
    if !site.set.is_empty() || site.rows_via_am {
        return None;
    }
    Some((relid, cols.iter().filter_map(|n| n.as_target_entry()?.expr.as_var().map(|v| v.varattno)).collect_in(px.mcx()), am == Am::Heap))
}

/// The run's locator word for a row at `l`: a heap row's TID word (`block << 16 | offset`, kernels ops/scan entries.rs
/// `tid_word`), a pgrc row's RowId (its `block` already carries the part and granule bits).
pub(crate) fn locator_word(b: &mut FunctionBuilder<'_>, l: Locator, heap: bool) -> Value {
    let block = if heap { let s = b.iconst(Type::I64, 16); b.int_bin(IntBinOp::Shl, l.block, s) } else { l.block };
    b.int_bin(IntBinOp::Or, block, l.offset)
}

/// The run row of the fetch form over the full row's `full`: each key's column once (by value), then the locator word.
pub(crate) fn narrow<'g>(full: &Shape, node: i32, mcx: Mcx<'g>) -> (Shape, PgVec<'g, u16>) {
    let mut keycols: PgVec<'_, u16> = PgVec::new_in(mcx);
    full.keys.iter().for_each(|k| if !keycols.contains(&k.col) { keycols.push_in(k.col) });
    let keys: Vec<Key> = full.keys.iter().map(|k| Key { col: keycols.iter().position(|&c| c == k.col).unwrap_or(0) as u16, ..*k }).collect(); // alloc: Program
    let mut cols: Vec<Col> = keycols.iter().map(|&c| full.cols[c as usize]).collect(); // alloc: Program
    if full.spec != 0 || cols.iter().any(|c| c.form != Form::Word) {
        bug(node, "a fetch top whose key is not a by-value word (topk_wide)");
    }
    let Some(tf) = row_of(INT8) else { bug(node, "no int8 type facts") };
    cols.push(Col { tf: *tf, form: Form::Word }); // alloc: Program
    let (nk, n) = (keys.len() as u32, cols.len() as u32);
    let rec_off = 16 * nk + 8 * n + n.next_multiple_of(8);
    (Shape { cols, keys, row_bytes: rec_off, spec: 0, rec_off }, keycols)
}

/// The producer's row for the run: its key columns (`keycols`, by the full row's column) and the locator `loc`.
pub(crate) fn run_row<'e>(row: &RowVals, keycols: &[u16], loc: Value, node: i32, mcx: Mcx<'e>) -> RowVals<'e> {
    let n = keycols.len();
    let mut out = RowVals { row: Row { cells: mcx::reserved_in(n + 1, mcx) }, vals: mcx::reserved_in(n + 1, mcx), cols: (0..=n as u16).map(Some).collect_in(mcx), carry: row.carry.copy_in(mcx), next: row.next, at: row.at };
    for &c in keycols.iter() {
        let Some(k) = row.cols.get(c as usize).copied().flatten().map(usize::from) else { bug(node, "a fetch key past the row") };
        out.row.cells.push_in(row.row.cells[k].clone());
        out.vals.push_in(row.vals[k]);
    }
    let life = CellLife { until: Until::Row, home: CellHome::RowCx };
    out.row.cells.push_in(Cell { rep: Rep::Word { bytes: 8 }, typid: INT8, num: NumTypmod::NONE, life, at: At::Locator, null: Nullable::Never });
    out.vals.push_in((loc, None));
    out
}

/// The emit: runs/sort.rs's reader over the `(key, locator)` run, its winners' columns fetched a window at a time (CAP-25):
/// the run's window cut to the site's request capacity (`MAX_FETCH_ROWS`), its locators gathered into the request lane and
/// fetched in one request (the site sorts them by RowId and decodes each touched granule once per column), then row `j`
/// reads its entry of each lane. The gathered rows `[from, to)` are frame words: a call stopped inside them (an out block
/// full every `OUT_ROWS` rows) reads them again from the lanes on its resume, never gathering again (a merged run's
/// `sort_next` hands each row once, in order). One request a row decoded a granule's columns again for every kept row: 6.8
/// us a row past a 20,000-row top, 4-12x the by-value form.
/// Under the top's Limit (no ties; `limit_first`), the rows its constant OFFSET skips are never fetched: the
/// Limit hands none of them on and reads none of their columns (ops/limit/lower.rs: OFFSET is a skip counter), so each
/// is the run's row with its columns NULL; and no row past `offset + count` (a constant count) is gathered, the Limit's end.
/// The fetch form then fetches the `count` rows the Limit hands on (fetch_shape.rs `fetchable_bound`).
pub(crate) struct FetchEmit<'g> {
    run: RunEmit,
    site: usize,
    tfs: PgVec<'g, TypeFacts>,
    /// The locator's byte offset in a run record (the narrow row's last column).
    loc_off: i32,
    /// The frame words of the gathered rows' first and end.
    w_from: i32,
    w_to: i32,
    /// The window's gathered first row (`next_window`'s), which `row` indexes the lanes from.
    from: Once<Option<Value>>,
    /// The top's Limit (no ties; `limit_first`): its constant OFFSET (the rows never fetched; 0: none) and `offset + count` when the
    /// count is a constant (no row at or past it is gathered).
    skip: i64,
    cap: Option<i64>,
}

/// The top's Limit (no ties) over pipeline `k`'s run: its constant offset (else 0) and `offset + count` for a constant
/// count (saturating). decompose's emit projects the Sort's own target list first (ops/sort/mod.rs: `emit` then
/// `ops::project`; map_limit pushes the Limit after it), so the Limit is the first op past projections, which pass every
/// row (the run's row `j` is the Limit's input row `j`) and, raise-free (D449's rule), give a skipped row's NULLs no
/// error PostgreSQL's real row would not.
fn limit_first(px: &Px<'_, '_>, k: usize) -> (i64, Option<i64>) {
    let ops = &px.gx.pipelines()[k].ops;
    let at = ops.iter().position(|op| !matches!(op, Op::Project(_))).unwrap_or(ops.len());
    if crate::props::ops_can_raise(px.gx, &ops[..at]) {
        return (0, None);
    }
    let Some(d) = ops.get(at).and_then(crate::frame::iface::demand).filter(|d| !d.ties) else { return (0, None) };
    let skip = match d.offset {
        Bound::Const(o) if *o > 0 => *o,
        _ => 0,
    };
    let cap = match d.count {
        Bound::Const(c) if *c >= 0 => Some(c.saturating_add(skip)),
        _ => None,
    };
    (skip, cap)
}

/// Pipeline `k`'s emit over the narrowed run `shape`; `full` the top's full row (its columns' types, entry order).
pub(crate) fn emit<'g>(px: &mut Px<'g, '_>, k: usize, shape: Shape, full: &Shape, relid: u32, attnos: &[i16]) -> FetchEmit<'g> {
    fires::fire("topk.fetch");
    let site = px.add_site(SiteDesc::range_source(px.arena, relid, attnos).with_lane(LaneDesc::rowid(), false));
    let loc_off = shape.datums() + 8 * (shape.cols.len() as i32 - 1);
    let mut word = |i: u8| px.frame_word(WordKey::Open { face: Face::Runs, k, j: u64::MAX, i }) as i32 * 8;
    let (w_from, w_to) = (word(runs_word::FETCH_FROM), word(runs_word::FETCH_TO));
    let tfs = full.cols.iter().map(|c| c.tf).collect_in(px.mcx());
    let (skip, cap) = limit_first(px, k);
    FetchEmit { run: RunEmit::over(px, k, shape), site, tfs, loc_off, w_from, w_to, from: Once::new(None), skip, cap }
}

impl SourceMech for FetchEmit<'_> {
    fn open_words(&self, w: &mut OpenWords) {
        self.run.open_words(w);
        w.declare(OpenWord::open("fetch from", Base::Words, self.w_from, Type::I64, 0));
        w.declare(OpenWord::open("fetch to", Base::Words, self.w_to, Type::I64, 0));
    }
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value { self.run.open(b, env) }
    fn write_back(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, cur: Value, out: Value) { self.run.write_back(b, env, cur, out) }

    /// The run's window from its cursor: inside the gathered rows (a resumed call), to their end; else rows gathered in
    /// order through the run's `sort_next` up to `cap`, the locators of those at or past `skip` into the request lane (at
    /// most `MAX_FETCH_ROWS`), then one `fetch_site` over them. A status below zero leaves through `l_eof`; a row the run does
    /// not have ends the window before it (none gathered: the run's end).
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let w = self.run.next_window(b, env, l_eof);
        let (from, to) = (env.load_word(b, self.w_from), env.load_word(b, self.w_to));
        let inside = { let a = b.icmp(IntCond::Sle, from, w.cursor); let z = b.icmp(IntCond::Slt, w.cursor, to); b.int_bin(IntBinOp::And, a, z) };
        let (l_gather, l_win) = (b.create_block(&[]), b.create_block(&[Type::I64, Type::I64]));
        b.cond_br(inside, (l_win, &[from, to]), (l_gather, &[]));
        b.switch_to(l_gather);
        // the first fetched row `s` (the cursor, or the Limit's offset past it) and the window's end: the run's, at most
        // `MAX_FETCH_ROWS` past `s`, and never past `cap`
        let skip = b.iconst(Type::I64, self.skip);
        let s = { let before = b.icmp(IntCond::Slt, w.cursor, skip); b.select(before, skip, w.cursor) };
        let far = { let room = b.iconst(Type::I64, MAX_FETCH_ROWS as i64); b.int_bin(IntBinOp::Add, s, room) };
        let mut end = { let past = b.icmp(IntCond::Sgt, w.end, far); b.select(past, far, w.end) };
        if let Some(c) = self.cap {
            let c = b.iconst(Type::I64, c);
            end = { let past = b.icmp(IntCond::Sgt, end, c); b.select(past, c, end) };
        }
        let slot = b.declare_stack_slot(8 * MAX_FETCH_ROWS as u32, 8);
        let ids = b.stack_addr(slot);
        let (l_head, l_body, l_got, l_none, l_bad) = (b.create_block(&[Type::I64]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        let l_req = b.create_block(&[Type::I64]);
        b.br(l_head, &[w.cursor]);
        b.switch_to(l_head);
        let j = b.block_params(l_head)[0];
        let more = b.icmp(IntCond::Slt, j, end);
        b.cond_br(more, (l_body, &[]), (l_req, &[j]));
        b.switch_to(l_body);
        let p = crate::face::runs::RunReader::fetch(&self.run, b, env, j);
        let pi = b.conv(ConvOp::PtrToInt, Type::I64, p);
        let (has, bad) = (b.icmp(IntCond::Sgt, pi, env.zero), b.icmp(IntCond::Slt, pi, env.zero));
        b.cond_br(has, (l_got, &[]), (l_none, &[]));
        b.switch_to(l_none);
        b.cond_br(bad, (l_bad, &[]), (l_req, &[j]));
        b.switch_to(l_bad);
        b.br(l_eof, &[pi]);
        b.switch_to(l_got);
        let j1 = b.int_bin(IntBinOp::Add, j, env.one);
        if self.skip > 0 {
            // a row the offset skips: its locator is not asked for
            let (wanted, l_keep) = (b.icmp(IntCond::Sge, j, s), b.create_block(&[]));
            b.cond_br(wanted, (l_keep, &[]), (l_head, &[j1]));
            b.switch_to(l_keep);
        }
        let loc = b.load(Type::I64, p, self.loc_off);
        let i = b.int_bin(IntBinOp::Sub, j, s);
        let at = b.ptr_add(ids, i, 8, 0);
        b.store(at, 0, loc);
        b.br(l_head, &[j1]);
        env.bounded.push_in(l_head); // the gather: at most MAX_FETCH_ROWS turns
        b.switch_to(l_req);
        let stop = b.block_params(l_req)[0];
        let (empty, l_some) = (b.icmp(IntCond::Sle, stop, w.cursor), b.create_block(&[]));
        b.cond_br(empty, (l_eof, &[env.zero]), (l_some, &[]));
        b.switch_to(l_some);
        // the rows to fetch: `stop - s`, none when the window holds only skipped rows
        let n = b.int_bin(IntBinOp::Sub, stop, s);
        let (none, l_fetch, l_ok) = (b.icmp(IntCond::Sle, n, env.zero), b.create_block(&[]), b.create_block(&[]));
        b.cond_br(none, (l_ok, &[]), (l_fetch, &[]));
        b.switch_to(l_fetch);
        let h = res::host(b, "fetch_site", &[Type::Ptr, Type::I64, Type::I64, Type::Ptr, Type::I64], Type::I64, crate::ops::sort::raw::fetch_site as *const () as usize);
        let (k, na) = (b.iconst(Type::I64, self.site as i64), b.iconst(Type::I64, self.tfs.len() as i64));
        let st = b.call_host(h, &[env.ctx, k, na, ids, n], Some(Type::I64)).expect("fetch_site returns a status");
        let neg = b.icmp(IntCond::Slt, st, env.zero);
        b.cond_br(neg, (l_eof, &[st]), (l_ok, &[]));
        b.switch_to(l_ok);
        env.store_word(b, self.w_from, w.cursor);
        env.store_word(b, self.w_to, stop);
        b.br(l_win, &[w.cursor, stop]);
        b.switch_to(l_win);
        let (from, to) = (b.block_params(l_win)[0], b.block_params(l_win)[1]);
        self.from.set(Some(from));
        Win { cursor: w.cursor, end: to, facts: w.facts }
    }

    /// Row `j`: entry `j - max(from, skip)` of each of the fetch site's lanes (the gathered rows' request), by the full
    /// row's entry order; a row the offset skips (`j < skip`) is every column NULL, read by nothing (the Limit skips it).
    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        let from = self.from.get().unwrap_or_else(|| bug(env.node, "a fetch row before its window"));
        let skip = b.iconst(Type::I64, self.skip);
        let first = { let before = b.icmp(IntCond::Slt, from, skip); b.select(before, skip, from) };
        let i = b.int_bin(IntBinOp::Sub, j, first);
        let skipped = (self.skip > 0).then(|| b.icmp(IntCond::Slt, j, skip));
        let arr = b.load(Type::Ptr, env.ctx, site_off(self.site) as i32 + site::LANES as i32);
        let n = self.tfs.len();
        let mut out = RowVals { row: Row { cells: mcx::reserved_in(n, env.mcx) }, vals: mcx::reserved_in(n, env.mcx), cols: (0..n as u16).map(Some).collect_in(env.mcx), carry, next, at: j };
        let (six, w63) = (b.iconst(Type::I64, 6), b.iconst(Type::I64, 63));
        let (word_i, bit_i) = (b.int_bin(IntBinOp::Ushr, i, six), b.int_bin(IntBinOp::And, i, w63));
        for (c, tf) in self.tfs.iter().enumerate() {
            let at = (c * lane::SIZE) as i32;
            // a skipped row's column: 0, NULL (no lane read)
            let (l_bits, l_val) = (b.create_block(&[Type::I64]), b.create_block(&[Type::I64, Type::I64]));
            if let Some(skipped) = skipped {
                let l_read = b.create_block(&[]);
                b.cond_br(skipped, (l_val, &[env.zero, env.one]), (l_read, &[]));
                b.switch_to(l_read);
            }
            let ptr = b.load(Type::Ptr, arr, at + lane::PTR as i32);
            let v = { let a = b.ptr_add(ptr, i, 8, 0); b.load(Type::I64, a, 0) };
            // entry i's validity bit; no validity words: every entry valid
            let valid = b.load(Type::Ptr, arr, at + lane::VALID as i32);
            let vi = b.conv(ConvOp::PtrToInt, Type::I64, valid);
            let none = b.icmp(IntCond::Eq, vi, env.zero);
            b.cond_br(none, (l_val, &[v, env.zero]), (l_bits, &[v]));
            b.switch_to(l_bits);
            let v = b.block_params(l_bits)[0];
            let w = { let a = b.ptr_add(valid, word_i, 8, 0); b.load(Type::I64, a, 0) };
            let bit = { let sh = b.int_bin(IntBinOp::Ushr, w, bit_i); b.int_bin(IntBinOp::And, sh, env.one) };
            let nb = b.int_bin(IntBinOp::Xor, bit, env.one);
            b.br(l_val, &[v, nb]);
            b.switch_to(l_val);
            let (v, nb) = (b.block_params(l_val)[0], b.block_params(l_val)[1]);
            let null = b.icmp(IntCond::Ne, nb, env.zero);
            // RUNS-11 / SRC-26: the fetch site publishes a column by its storage class (hosts pgrc fetch_site.rs): every
            // varlena and (pgrc) fixed-length by-reference column as a pointer to its image, every by-value class as its
            // datum word: the type's storage form
            let rep = Rep::of_storage(tf.typlen, tf.byval);
            let life = CellLife { until: Until::Row, home: CellHome::FrameSlot };
            out.row.cells.push_in(Cell { rep, typid: tf.typid, num: NumTypmod::NONE, life, at: At::Slot(c as u16), null: Nullable::Maybe });
            out.vals.push_in((v, Some(null)));
        }
        out
    }
}

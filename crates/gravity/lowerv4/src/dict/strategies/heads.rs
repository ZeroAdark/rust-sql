//! Road `code.heads` (cards/M6.md L4, H11, R4-L07, row I34; V4R-310): a key-only order over one pgrc coded text column,
//! either direction, under a collation where equal values are equal bytes (RUNS-15, B3-24, AGA-14: the top
//! orders the entries by its own comparator, so neither the codes' byte order nor the direction matters; a
//! nondeterministic collation's equal values are distinct entries, which the top would order apart from PostgreSQL's
//! sort), reads each safe part's dictionary entries instead of its rows. The
//! producer's site publishes its code lane as entries (source/dict_entries.rs, asked through `entries`; the host serves a
//! part as entries only while `entries_safe`: delete-free, and not under a transaction snapshot), so the run holds each
//! surviving `(part, code)` once, each row marked (`Mark`: bit 63, the part's first granule, the code; 0 for a row read
//! as rows). Every entry is referenced by at least one visible row of its part, so the top's first `n` entries in its
//! order hold the first `n` rows. The emit (`Emit`, over runs.topk_rows's reader) sends a marked row as many times as its part holds its
//! code, counted at the row through a count site over the key column (`heads_count`), so only rows the Limit reads are
//! counted, each part's codes all at once on its first entry (D3096: the kernel's per-part table behind the memo word,
//! pinned for the statement), so a deep OFFSET costs one pass per part, not one per entry. B3-24 / RUNS-15: a nullable key's part holding NULLs adds one NULL row after its entries (the site's
//! `entries_nulls`, dict_entries.rs), marked with `NULL_CODE`; the top places it by the key's NULLS FIRST / LAST like any
//! row and the emit sends it once per NULL row of its part (kernels heads `count`: the lane's invalid rows).
//! v2 hosts/heads.rs + source/emit_topk.rs, the ladder's `lower_heads`. Budget 160 lines. Owner M6-CODES.
use gravity_abi::ctx::{lane, scanpos, site_off};
use gravity_abi::site::site;
use gravity_decompose::{Am, Op, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_program::{LaneId, NumTypmod, Rep, SiteDesc, Until};
use mcx::{CollectIn, GrowIn, PgBox, PgVec};
use types_nodes::primnodes::OUTER_VAR;

use crate::cell::{At, Cell, CellHome, CellLife, Nullable};
use crate::face::end::EndMech;
use crate::face::kinds::{Face, KindId};
use crate::face::runs::{CodeTopkImpl, CodeTopkMech, RunReader, TopDesc};
use crate::face::source::{SourceMech, WinConds};
use crate::frame::env::Env;
use crate::frame::frame::Win;
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{Px, WordKey};
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::graph::Gx;
use crate::ops::Table;

crate::strategies! {
    /// Dictionary entries as the source of a key-only order.
    CODE_HEADS = 81, "code.heads", CodeTopk;
}

pub(crate) const ROWS: Table = Table { strategies: &[(CODE_HEADS, &HEADS)], ..crate::ops::NONE };
const HEADS: CodeTopkImpl = CodeTopkImpl(&Heads);
/// The emit's frame words (`WordKey::Open { face: Runs, k, j: WORDS_J, i }`): the held row, its sends left, the window's
/// start, and (`i: 3`, not an open word: the execution's reset zeroes it, no re-open does) the kernel's per-part count
/// tables' directory (D3096).
const WORDS_J: u64 = u64::MAX - 2;
const MARK: i64 = 1 << 63;
/// The mark's code of a part's NULL row (no dictionary code: a part's codes stay below pgrc's 2^24 cap).
const NULL_CODE: i64 = gravity_kernels::dict::strategies::heads::NULL_CODE as i64;

struct Heads;

/// Whether producer `k`'s engaged top keys a nullable column (its site asks for each part's NULL row: dict_entries.rs).
pub(crate) fn nullable_key(gx: &Gx<'_, '_>, k: usize) -> bool {
    let p = &gx.pipelines()[k];
    let Some(Op::Sort(sd)) = p.ops.iter().find(|op| matches!(op, Op::Sort(_))) else { return false };
    let top = TopDesc { k, node: p.node_id, sd };
    key(gx, &top).is_some_and(|(relid, attno, _)| !gx.facts.att(relid, attno).is_some_and(|f| f.notnull))
}

/// Producer `k`'s engaged top: `(relation, the key's scan attno, the key's column in the run row)`. D3844: never a member of
/// an owner's rerun set (a correlated SubPlan's top): its run keeps rows, read by RunEmit, which re-opens per run (C-R6f).
fn key(gx: &Gx<'_, '_>, top: &TopDesc<'_, '_>) -> Option<(u32, i16, i16)> {
    let p = &gx.pipelines()[top.k];
    let Source::Range { am: Am::Pgrcolumnar, relid, .. } = p.source else { return None };
    let tes: PgVec<'_, _> = top.sd.tlist.iter().filter_map(|n| n.as_target_entry()).collect_in(gx.mcx);
    let ([k0], [te]) = (top.sd.keys.as_slice(), &tes[..]) else { return None };
    let at = te.expr.as_var().filter(|v| v.varno == OUTER_VAR && te.resno == k0.resno)?.varattno;
    let text = k0.image.text_like() && k0.coll.bytes_eq && !k0.compare_fn(); // USEROP: not a user family's order
    let plain = p.ops.iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_) | Op::Sort(_)));
    let proj = p.ops.iter().rev().find_map(|op| if let Op::Project(l) = op { Some(*l) } else { None })?;
    let attno = proj.iter().filter_map(|n| n.as_target_entry()).find(|te| te.resno == at)?.expr.as_var()?.varattno;
    let member = crate::rerun::sets::in_owner_set(gx, top.k);
    (text && plain && !member && gx.facts.att(relid, attno).is_some() && gx.end_need(top.k) == KindId::RunsTopkRows).then_some((relid, attno, at))
}

impl CodeTopkMech for Heads {
    fn engages(&self, gx: &Gx<'_, '_>, top: &TopDesc<'_, '_>) -> bool {
        key(gx, top).is_some()
    }
    fn marks(&self) -> bool {
        true
    }
    fn entries(&self, px: &Px<'_, '_>, k: usize, site: &SiteDesc) -> bool {
        let Some(Op::Sort(sd)) = px.gx.pipelines()[k].ops.iter().find(|op| matches!(op, Op::Sort(_))) else { return false };
        let top = TopDesc { k, node: px.gx.pipelines()[k].node_id, sd };
        key(px.gx, &top).is_some_and(|(_, attno, _)| site.lanes().first().is_some_and(|l| l.id == LaneId::Attr(attno)))
    }
    fn end<'g>(&self, px: &mut Px<'g, '_>, top: &TopDesc<'_, '_>, inner: PgBox<'g, dyn EndMech + 'g>) -> PgBox<'g, dyn EndMech + 'g> {
        let Some((relid, attno, at)) = key(px.gx, top) else { return inner };
        // D3027 (B): the count pass after the top; the mark is the run row's last datum word (its one key's pair first,
        // runs/topk_rows.rs `coded`'s shape: the projection's columns, then the mark)
        let ncols = crate::ops::sort::lower::sort_of(px, top.k).1.len() as u32;
        crate::dict::strategies::heads_pass::add(px, top.k, relid, attno, top.node, 16 + 8 * ncols);
        mcx::pg_box_dyn!(px.mcx(), Mark { inner, at, node: top.node } => dyn EndMech + 'g)
    }
    /// `top.k` is the reader; its producer's key column and site give the count site's lane.
    fn emit<'g>(&self, px: &mut Px<'g, '_>, top: &TopDesc<'_, '_>, reader: &mut dyn FnMut() -> PgBox<'g, dyn RunReader + 'g>) -> Option<PgBox<'g, dyn SourceMech + 'g>> {
        let gx = px.gx;
        let Source::Breaker { slot } = gx.pipelines()[top.k].source else { return None };
        let Some(producer) = gx.life_of(top.k, slot).map(|l| l.0 as usize) else { return None };
        let Some((relid, attno, _)) = key(gx, &TopDesc { k: producer, ..*top }) else { return None };
        let lane = px.pipeline_site(producer).and_then(|i| px.sites.get(i)?.lanes().iter().find(|l| l.id == LaneId::Attr(attno)).copied());
        let Some(ld) = lane.filter(|l| l.kind == lane::KIND_CODES) else { return None };
        let mut s = SiteDesc::range_source(px.arena, relid, &[attno]);
        s.lanes[0] = ld;
        let site = px.add_site(s);
        let mut w = |i: u8| px.frame_word(WordKey::Open { face: Face::Runs, k: top.k, j: WORDS_J, i }) as i32 * 8;
        let (w_p, w_left, w_x, w_memo) = (w(0), w(1), w(2), w(3));
        let dir = crate::dict::strategies::heads_pass::dir_word(px, producer);
        Some(mcx::pg_box_dyn!(px.mcx(), Emit { inner: reader(), site, w_p, w_left, w_x, w_memo, dir } => dyn SourceMech + 'g))
    }
}

/// The producer's end over the top's: the row goes on with its mark past the projection's columns.
struct Mark<'g> {
    inner: PgBox<'g, dyn EndMech + 'g>,
    at: i16,
    node: i32,
}

impl EndMech for Mark<'_> {
    fn state_bytes(&self) -> u32 { self.inner.state_bytes() }
    fn open_words(&self, w: &mut OpenWords) { self.inner.open_words(w) }
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) { self.inner.open(b, env) }
    fn carried<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>) -> PgVec<'e, Value> { self.inner.carried(b, env) }
    fn window_end(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, c: &Carry) { self.inner.window_end(b, env, c) }
    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) { self.inner.finish(b, env, x) }
    fn sink(&self) -> crate::face::end::SinkKind { self.inner.sink() }
    fn wants_locator(&self) -> bool { self.inner.wants_locator() }
    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize, d: gravity_program::Pipeline) -> gravity_program::Pipeline { self.inner.pipeline_desc(px, k, d) }

    /// An entries window's row (its code lane's `FLAG_ENTRIES`) is marked `bit 63 | first granule << 32 | code` (a part's
    /// NULL row: `NULL_CODE` for the code); any other 0.
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, x: &Exits) -> Carry<'e> {
        let (Some(cr), Some(s)) = (row.col(self.at), env.site) else { crate::refuse!(self.node, crate::admit::refusal::Reason::Bug { payload: "a heads mark without its key or site" }) };
        let mark = match cr.cell.rep {
            Rep::Code { .. } => {
                let mut cx = env.lx(b, self.node);
                let code = crate::rep::code_word(&mut cx, cr);
                env.memo = cx.memo;
                let lanes = b.load(Type::Ptr, env.ctx, (site_off(s) + site::LANES) as i32);
                let fl = b.load_zext(Type::I8, lanes, lane::FLAGS as i32);
                let bit = b.iconst(Type::I64, lane::FLAG_ENTRIES as i64);
                let ent = { let m = b.int_bin(IntBinOp::And, fl, bit); b.icmp(IntCond::Ne, m, env.zero) };
                let blk = b.load_zext(Type::I32, env.ctx, (site_off(s) + site::POS + scanpos::BLOCK) as i32);
                let g = b.load_zext(Type::I32, env.ctx, (site_off(s) + site::GRANULE) as i32);
                let (start, s32, top) = (b.int_bin(IntBinOp::Sub, blk, g), b.iconst(Type::I64, 32), b.const_raw(Type::I64, MARK as u64));
                let hi = b.int_bin(IntBinOp::Shl, start, s32);
                let code = match cr.null {
                    Some(nl) => { let nc = b.iconst(Type::I64, NULL_CODE); b.select(nl, nc, code) }
                    None => code,
                };
                let m = { let t = b.int_bin(IntBinOp::Or, top, hi); b.int_bin(IntBinOp::Or, t, code) };
                b.select(ent, m, env.zero)
            }
            _ => env.zero,
        };
        let mut r = RowVals { row: row.row.clone(), vals: row.vals.clone(), cols: row.cols.clone(), carry: row.carry.copy_in(env.mcx), next: row.next, at: row.at };
        let life = CellLife { until: Until::Row, home: CellHome::RowCx };
        let i = r.row.cells.len() as u16;
        r.row.cells.push_in(Cell { rep: Rep::Word { bytes: 8 }, typid: 20, num: NumTypmod::NONE, life, at: At::Value(i), null: Nullable::Never });
        r.vals.push_in((mark, None));
        r.cols.push_in(Some(i));
        self.inner.lower(b, env, &r, x)
    }
}

/// The top's emit: the reader's rows in order, a marked one sent once per row of its code in its part.
struct Emit<'g> {
    inner: PgBox<'g, dyn RunReader + 'g>,
    site: usize,
    w_p: i32,
    w_left: i32,
    w_x: i32,
    w_memo: i32,
    /// D3027 (B): the count pass's directory word (heads_pass.rs), when the producer has the pass.
    dir: Option<u16>,
}

impl SourceMech for Emit<'_> {
    fn open_words(&self, w: &mut OpenWords) {
        self.inner.open_words(w);
        for (n, at) in [("heads row", self.w_p), ("heads sends left", self.w_left), ("heads window start", self.w_x)] {
            w.declare(OpenWord::open(n, Base::Words, at, Type::I64, 0));
        }
    }
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value { self.inner.open(b, env) }

    /// With sends left, the held row's window; else the reader's next row fetched (the only fetch: a merge moves on at
    /// each), its sends counted, the reader's cursor past it.
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let left = env.load_word(b, self.w_left);
        let (l_fetch, l_row, l_cnt, l_set, l_have) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::I64]), b.create_block(&[]));
        let has = b.icmp(IntCond::Sgt, left, env.zero);
        b.cond_br(has, (l_have, &[]), (l_fetch, &[]));
        b.switch_to(l_fetch);
        let w = self.inner.next_window(b, env, l_eof);
        let p = self.inner.fetch(b, env, w.cursor);
        let pi = b.conv(ConvOp::PtrToInt, Type::I64, p);
        let ok = b.icmp(IntCond::Sgt, pi, env.zero);
        b.cond_br(ok, (l_row, &[]), (l_eof, &[pi])); // null: the end (0); below zero: its status
        b.switch_to(l_row);
        let r = self.inner.vals(b, env, p, Carry::new(env.zero, env.mcx), w.cursor, w.cursor);
        let Some(&(mark, _)) = r.vals.last() else { crate::refuse!(env.node, crate::admit::refusal::Reason::Bug { payload: "a heads run row without its mark" }) };
        let ent = b.icmp(IntCond::Slt, mark, env.zero);
        b.cond_br(ent, (l_cnt, &[]), (l_set, &[env.one]));
        b.switch_to(l_cnt);
        let mem = HostFlags { touches_memory: true, ..HostFlags::default() };
        let h = b.declare_host_once("heads_count", &[Type::Ptr, Type::I64, Type::I64, Type::Ptr, Type::Ptr], Some(Type::I64), gravity_kernels::dict::strategies::heads::heads_count as *const () as usize, mem);
        let k = b.iconst(Type::I64, self.site as i64);
        let words = env.words.unwrap_or_else(|| crate::refuse!(env.node, crate::admit::refusal::Reason::Bug { payload: "a heads emit without its frame words" }));
        let memo = b.ptr_add(words, env.zero, 1, self.w_memo);
        // D3027 (B): the count pass's directory (null below width 2, or without the pass)
        let dir = match self.dir {
            Some(wd) => b.load(Type::Ptr, env.params, 8 * wd as i32),
            None => b.conv(ConvOp::IntToPtr, Type::Ptr, env.zero),
        };
        let c = b.call_host(h, &[env.ctx, k, mark, memo, dir], Some(Type::I64)).expect("heads_count returns the rows");
        let bad = b.icmp(IntCond::Slt, c, env.zero);
        b.cond_br(bad, (l_eof, &[c]), (l_set, &[c]));
        b.switch_to(l_set);
        let c = b.block_params(l_set)[0];
        env.store_word(b, self.w_p, pi);
        env.store_word(b, self.w_left, c);
        let u = b.int_bin(IntBinOp::Add, w.cursor, env.one);
        self.inner.write_back(b, env, u, env.zero);
        b.br(l_have, &[]);
        b.switch_to(l_have);
        let (x, left) = (env.load_word(b, self.w_x), env.load_word(b, self.w_left));
        Win { cursor: x, end: b.int_bin(IntBinOp::Add, x, left), facts: WinConds::default() }
    }

    /// Row `j`: the held row's cells, its mark dropped.
    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        let pi = env.load_word(b, self.w_p);
        let p = b.conv(ConvOp::IntToPtr, Type::Ptr, pi);
        let mut r = self.inner.vals(b, env, p, carry, next, j);
        r.row.cells.pop();
        r.vals.pop();
        r.cols.pop();
        r
    }

    fn write_back(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, cur: Value, _out: Value) {
        let (x, left) = (env.load_word(b, self.w_x), env.load_word(b, self.w_left));
        let sent = b.int_bin(IntBinOp::Sub, cur, x);
        let left = b.int_bin(IntBinOp::Sub, left, sent);
        env.store_word(b, self.w_left, left);
        env.store_word(b, self.w_x, cur);
    }
}

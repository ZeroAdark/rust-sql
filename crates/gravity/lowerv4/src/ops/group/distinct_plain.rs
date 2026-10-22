//! Count(DISTINCT x) with no group key, kind `keyed.distinct_plain` (R3-L06 `distinct.total`):
//! a plain aggregate whose DISTINCT argument decompose made the group's one key (resno 0). Two forms:
//! - the count form, when every aggregate counts the argument (`PairCount`): each row's non-NULL argument is imaged and
//!   upserted into a key-only table (TABLES' resolver picks the hashed table or a code front), NULL rows skipped; the
//!   owner phase dedups each partition and sums the lengths (keyed_distinct_total's owner), and the emit reads the one
//!   accumulator record's pairs slot (`pairs_total`);
//! - the pair form, when other aggregates ride along (count(*), sum(w)) or a pair folds the argument (sum / avg / min /
//!   max(DISTINCT int), `PairFold`): the hash aggregate's end keyed on the argument (keyed_hashagg; the NULL argument is a
//!   record too, so the riders count its rows), and the emit walks every record, counting / folding each non-NULL key once
//!   and merging the riders' slots into one accumulator.
//! The emit is one row, as PG's plain aggregate: over an empty input a count is 0 and a fold NULL.
//! DC-21's row count (`row`: count(DISTINCT ROW(k1, .., kn)), normalize's spelling of count(*) over a grouping by a
//! nullable key or by several keys) is the count form over the row's fields as the keys, their flags words kept and no
//! row skipped, with neither the derived code column nor the window host (each reads one key and skips its NULLs).
//! The count form's window host (V4R-26 item 1, the window phase): over a scan column of a signed integer image with no
//! op before the end, a window of the bypassed table is taken whole (`EndMech::whole`) by `agg_distinct_window` (host_abi
//! v4 m1_keyed.rs): the adjacent duplicates dropped and each key's pair record appended to its partition in one host loop,
//! where the rows would carry the run and append it in IR; a window the host declines walks its rows. It rides
//! run.collapse (the collapse's window form): off with it. A covered window in table mode takes the host where the
//! worker's last measured window of the end came in runs of equal keys (NEW-40: the runs word the host writes, re-measured
//! one window in `RUNS_PROBE`). Budget 450 lines (files.tsv, +25 % gate). Owner M1-KEYED.
//! Hosts: agg_distinct_window (host_abi v4 m1).
use gravity_abi::breaker::{RecordLayout, ACC_PAIRS};
use gravity_abi::ctx::{hdr, RESUME_NONE};
use gravity_abi::status::{unsupported, UNSUPPORTED_SCRATCH};
use gravity_decompose::ops::shared::agg::{AggDecl, FinalForm, GroupDecl, NONE};
use gravity_decompose::Op;
use gravity_expr::emit::hash::emit_key_hash;
use gravity_expr::emit::{AggCell, CellValid};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, HostFlags, IntBinOp, IntCond, IntUnOp, Type, Value};
use gravity_kernels::host_abi::{agg_gather, agg_gather_at, pairs_total_at, GATHER_FRESH};
use gravity_kernels::util::RESUME_OPEN;
use gravity_program::{NumTypmod, Pipeline, Rep, Until};
use mcx::{Mcx, PgBox, PgVec};
use mcx::GrowIn;

use crate::cell::{At, Cell, CellHome, CellLife, Nullable, Row};
use crate::face::end::EndMech;
use crate::face::fold::{Acc, FoldClass};
use crate::dict::face::CodeRep;
use crate::table::face::{RunCarry, RunCarryImpl, RunFold, TableDecl, TableImpl, TableMech};
use crate::table::keyimg::ImageWords;
use crate::face::kinds::{Face, KindId};
use crate::face::strategy::FRAME_CARRY;
use crate::face::runs::OrderUse;
use crate::face::source::{SourceMech, WinConds};
use crate::frame::cancel;
use crate::frame::env::{AggRow, Env};
use crate::frame::frame::{EndMaker, SourceMaker, Win};
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{ExId, Px, WordKey};
use crate::frame::row::{Carry, Mask, RowVals, WordWin};
use crate::frame::stop::Exits;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::rep::CellRef;
use mcx::CollectIn;

use crate::table::emit_group::{ES_KEPT, ES_PTRS};
use crate::ops::agg::hashagg::GroupRec;

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table {
    kinds: &[(KindId::KeyedDistinctPlain, &EndMaker(make_end)), (KindId::KeyedDistinctPlain, &SourceMaker(make_emit))],
    ..crate::ops::NONE
};

fn bug(node: i32, payload: &'static str) -> ! {
    crate::refuse!(node, Reason::Bug { payload })
}

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Form {
    Count,
    Pairs,
}

fn pair(d: &AggDecl<'_>) -> bool {
    matches!(d.final_, FinalForm::PairCount | FinalForm::PairFold)
}

/// DC-21: the group counts the distinct rows of its keys (`count(DISTINCT ROW(k1, .., kn))`, decompose agg `row_count`:
/// every call a `PairCount` with `mods.star`, the row's fields the resno-0 keys): a row is never NULL, so no row is
/// skipped, and each key's NULL is a value of the record (its flags word), as a GROUP BY's NULL group is.
fn row(g: &GroupDecl<'_>) -> bool {
    g.aggs.iter().any(|d| d.final_ == FinalForm::PairCount && d.mods.star)
}

/// The group's form. A pair's FILTER is the pair set's (V4R-367: hashagg_rec.rs `pair_filter`), a pair fold folds an
/// integer key word, and every other aggregate has a record slot (a second DISTINCT argument has none: decompose keeps one pair set, so it refuses
/// NotBuilt M6 keyed.distinct_multi by name).
fn form(g: &GroupDecl<'_>, node: i32) -> Form {
    if row(g) {
        // DC-21: the row's fields, every call the row count (decompose keys the fields only where every call is one)
        if g.keys.is_empty() || g.keys.iter().any(|k| k.resno != 0) || !g.aggs.iter().all(|d| d.final_ == FinalForm::PairCount && d.mods.star && d.filter.is_none()) {
            bug(node, "a DISTINCT row count beside a key or a call that is not its own");
        }
        if g.empty_no_row || !g.pinned.is_empty() {
            shape(node, "a plain DISTINCT aggregate whose group keys a filter pins");
        }
        return Form::Count;
    }
    if g.keys.len() != 1 || g.keys[0].resno != 0 {
        bug(node, "a plain distinct group without its one DISTINCT key");
    }
    if g.empty_no_row || !g.pinned.is_empty() {
        shape(node, "a plain DISTINCT aggregate whose group keys a filter pins");
    }
    let int_key = matches!(g.keys[0].kind, gravity_abi::breaker::KeyKind::I64 | gravity_abi::breaker::KeyKind::I32);
    let mut f = Form::Count;
    for d in g.aggs.iter() {
        match d.final_ {
            FinalForm::PairCount => {}
            FinalForm::PairFold if !int_key => shape(node, "a DISTINCT fold over a key that is not an integer word"),
            _ if !pair(d) && d.slot == NONE => crate::not_built!(node, crate::admit::refusal::Variant::Kind(KindId::KeyedDistinctMulti)),
            _ => f = Form::Pairs,
        }
    }
    f
}

/// The group's record in the count form: the argument's image alone (no flags word: NULL rows never reach the table);
/// DC-21's row count keeps its keys' flags words (a NULL field is a value of the row).
/// A DISTINCT argument read as its image alone (keyed_hashagg `Arg::Key`) over `row`: its per-entry `Fp128` cell
/// (step_percode_key, made at lower time), else the argument evaluated as a cell of `cell`'s kind.
pub(crate) fn key_val(b: &mut FunctionBuilder<'_>, env: &mut Env, node: i32, row: &RowVals, id: ExId, cell: Cell) -> crate::ops::agg::hashagg::KeyVal {
    if let Some((c, val, null)) = crate::dict::strategies::percode_key::cell(b, env, node, row, id) {
        return (c, val, Some(null));
    }
    let (val, null) = crate::ops::agg::hashagg::ex_val(b, env, node, row, id);
    (cell, val, Some(null))
}

fn count_rec<'g, 'p>(px: &mut Px<'g, 'p>, g: &'g GroupDecl<'p>) -> GroupRec<'g, 'p> {
    let mut rec = GroupRec::new(px, g);
    if row(g) {
        rec.layout = RecordLayout::with_sentinel(rec.layout.key_kinds(), &[], false, None);
        return rec;
    }
    let n = rec.layout.nkeys() - rec.flag_count();
    rec.layout = RecordLayout::with_sentinel(&rec.layout.key_kinds()[..n], &[], false, None);
    rec.nullable = false;
    rec
}

fn group<'g, 'p>(px: &Px<'g, 'p>, k: usize, drained: bool) -> (&'g GroupDecl<'p>, i32) {
    let node = px.gx.pipelines()[k].node_id;
    match if drained { px.gx.drained_group(k) } else { px.gx.group_at(k) } {
        Some(g) => (g, node),
        None => bug(node, "a plain distinct pipeline without its group"),
    }
}

fn make_end<'g, 'p>(px: &mut Px<'g, 'p>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    let (g, node) = group(px, k, false);
    // PSP2-34: count(DISTINCT) over several arguments, each its own key, is keyed.distinct_multi's one-pass form; D536: a
    // DISTINCT aggregate past the first argument (no record slot) is keyed.distinct_multi's group (M6 L0-e)
    if crate::ops::group::distinct_multi::multi(g) {
        crate::mechs_used::declined(crate::admit::refusal::Variant::Kind(KindId::KeyedDistinctPlain)); // MECHCORPUS: the node is another kind's form
        return (crate::frame::iface::kind_impl::<EndMaker>(&node, KindId::KeyedDistinctMulti).0)(px, k);
    }
    if g.aggs.iter().any(|d| !pair(d) && d.slot == NONE) {
        if g.aggs.iter().any(|d| pair(d) && d.filter.is_some()) {
            shape(node, "a FILTER on a DISTINCT aggregate beside a second DISTINCT argument");
        }
        crate::mechs_used::declined(crate::admit::refusal::Variant::Kind(KindId::KeyedDistinctPlain)); // MECHCORPUS: the node is another kind's form
        return (crate::frame::iface::kind_impl::<EndMaker>(&node, KindId::KeyedDistinctMulti).0)(px, k);
    }
    if !crate::graph::order_free(g) { // D859: PG feeds every aggregate in the DISTINCT argument's sorted order
        shape(node, "a rider whose value reads the row order beside a DISTINCT");
    }
    if form(g, node) == Form::Pairs {
        return crate::ops::agg::hashagg::make(px, k);
    }
    let mut rec = count_rec(px, g);
    rec.compile_args(px, true);
    let t = crate::ops::agg::hashagg::table_decl(px, k, &rec, 0);
    let table = crate::table::face::table(&node, &t);
    // DC-21: a row count reads no derived code column and takes no window host (both read one key and skip its NULLs)
    let row = row(g);
    let (run, src) = (run_carry(&node, &t).filter(|_| crate::ops::shared::agg::carry_rides(px, k, crate::ops::shared::agg::KEYED_RATE)), (!row).then(|| crate::table::emit_group::derived_col(px.gx, &rec, &t)).flatten());
    let win = (src.is_none() && !row).then(|| win_host(px, k, &rec, &t, false)).flatten();
    let blk = (src.is_none() && !row).then(|| win_host(px, k, &rec, &t, true)).flatten();
    let filters = px.gx.pipelines()[k].ops.iter().enumerate().filter(|(_, o)| matches!(o, Op::Filter(_))).fold(0u64, |m, (j, _)| m | 1u64.checked_shl(j as u32).unwrap_or(u64::MAX));
    let covered = filters != 0 && filters & !crate::ops::shared::scan::covered_ops(px, k) == 0;
    let word = |px: &mut Px<'_, '_>, i: u8| px.frame_word(WordKey::Open { face: Face::Keyed, k, j: u64::MAX, i }) as i32 * 8;
    let runs = (covered && blk.is_some()).then(|| RunsWords { scratch: table.state_bytes(&t), runs: word(px, W_RUNS), left: word(px, W_LEFT) });
    mcx::pg_box_dyn!(px.mcx(), CountEnd { rec, t, table, run, src, win, blk, covered, runs, row, mcx: px.mcx() } => dyn EndMech + 'g)
}

/// NEW-40: the windows a covered table-mode window walks between two measures of the runs word (`CountEnd::whole`): the
/// window past them takes the host, which measures again. CHOSEN, not measured: over keys with no runs one window in
/// `RUNS_PROBE + 1` pays the host's loss there (10-19 % of that window), and runs that start part way through the end are
/// found within `RUNS_PROBE` windows; it moves no answer (both roads upsert the same keys).
const RUNS_PROBE: i64 = 16;

/// NEW-40: KEYED's frame words of the end's pipeline that keep the runs signal across the statement's claims (past
/// codefront_light's `W_DECLINED`): a claim never crosses a part's edge and every claim's open clears the end's state, so a
/// word there would measure again at each claim's first window; a frame word is zero at the statement's bind, the worker's
/// own. `W_RUNS`: the runs word of the worker's last measured window (kernels entries_distinct.rs `runs_word`); `W_LEFT`:
/// the windows left to walk before the next measure (0 at the bind: the first covered window measures).
const W_RUNS: u8 = 17;
const W_LEFT: u8 = 18;

/// NEW-40: where the end's runs signal lives (`whole`'s table-mode branch).
#[derive(Clone, Copy)]
struct RunsWords {
    /// The state word past the table's state the window host writes (the arg's offset above bit 32), copied to `runs`
    /// after each taken window; the open fills it from `runs`, so a window with no non-NULL row copies the word back as it was.
    scratch: u32,
    /// `W_RUNS`'s and `W_LEFT`'s byte offsets in the frame words.
    runs: i32,
    left: i32,
}

/// A keyed end's run carry (`run.collapse`) over table `t`, unless a code front takes `t`: the front folds each row into
/// its code's L2 record, while a run the carry closes reaches the table as a plain image (no code, no index) and takes the
/// hashed probe, once per run: a run per row on an unsorted code key (a dictionary-coded text argument).
pub(crate) fn run_carry(node: &i32, t: &TableDecl) -> Option<&'static dyn RunCarry> {
    let front = [CodeRep::FrontDense, CodeRep::FrontPart].into_iter().any(|r| r.on() && r.need::<TableImpl>(node).0.takes(t));
    (!front && OrderUse::Collapse.on()).then(|| OrderUse::Collapse.need::<RunCarryImpl>(node).0)
}

/// V4R-26 item 1: the count form's window host over pipeline `k`: the argument's site lane (`at`'s low byte) and the host's
/// word (`agg_distinct_window`'s `arg`: the type length, the dense flag's offset in the state or 0), when the argument is a
/// column of the pipeline's pgrc scan (through its projection) of a Raw integer image (KEYED-24: signed or not), the pipeline's ops are projections,
/// and the table's state is an `AggState` (the hashed table, or table.small over the hashed one); no pair FILTER (V4R-367).
/// `block` (KEYED-24, `agg_distinct_block`): the pipeline's ops may also be Filters (the frame's block path masks their
/// rows); a block the host declines at run time (a dense table.small) walks its rows (`CountEnd::block_or_walk`).
fn win_host(px: &mut Px<'_, '_>, k: usize, rec: &GroupRec<'_, '_>, t: &TableDecl, block: bool) -> Option<(u8, u64)> {
    use crate::table::face::TablePolicy;
    use types_nodes::primnodes::OUTER_VAR;
    // the resolver's (face/keyed.rs `table`) policies ahead of identity / bypass, whose state is not an `AggState`
    let node = rec.node;
    let front = [CodeRep::FrontDense, CodeRep::FrontPart].into_iter().any(|r| r.on() && r.need::<TableImpl>(&node).0.takes(t));
    let small = TablePolicy::DenseSmall.on() && TablePolicy::DenseSmall.need::<TableImpl>(&node).0.takes(t);
    let dense_off = if small { crate::table::bypass::HASHED.state_bytes(t) as u64 } else { 0 };
    let p = &px.gx.pipelines()[k];
    // a pgrc range scan: its windows are decoded lanes (a heap window's lanes may be raw pages)
    let gravity_decompose::Source::Range { scanrelid, am: gravity_decompose::Am::Pgrcolumnar, .. } = &p.source else { return None };
    let (key, tf) = (rec.g.keys.first()?, rec.tfs.first()?);
    let ops = p.ops.iter().all(|o| matches!(o, Op::Project(_)) || (block && matches!(o, Op::Filter(_))));
    // KEYED-24: either Raw image: a signed one sign-extends the lane's bytes, an unsigned one (oid) zero-extends them
    let Some(signed) = tf.image.int_signed() else { return None };
    if front || rec.pair_filter.is_some() || rec.g.keys.len() != 1 || key.words != 1 || !matches!(tf.typlen, 2 | 4 | 8) || !ops {
        return None;
    }
    let mut v = crate::table::emit_group::bare(key.expr.node()).as_var()?;
    if v.varno == OUTER_VAR {
        let Some(Op::Project(l)) = p.ops.iter().rev().find(|op| matches!(op, Op::Project(_))) else { return None };
        v = crate::table::emit_group::bare(l.iter().filter_map(|n| n.as_target_entry()).find(|te| te.resno == v.varattno)?.expr).as_var()?;
    }
    if v.varno != *scanrelid as i32 || v.varlevelsup != 0 || v.varattno <= 0 {
        return None;
    }
    let lane = px.site_mut(k)?.lane_index(gravity_program::LaneId::Attr(v.varattno)).filter(|&l| l < 256)?;
    let unsigned = if signed { 0 } else { gravity_kernels::ops::group::entries_distinct::DW_UNSIGNED };
    Some((lane as u8, tf.typlen as u64 | unsigned | dense_off << 16))
}

/// The count form's end: the non-NULL argument into the key-only table (a run of equal arguments upserts once).
struct CountEnd<'g, 'p> {
    rec: GroupRec<'g, 'p>,
    t: TableDecl,
    table: &'static dyn TableMech,
    run: Option<&'static dyn RunCarry>,
    /// A derived argument's code column (keyed_emit_group `derived_col`).
    src: Option<i16>,
    /// The window host's `(site lane, arg)` (`win_host`).
    win: Option<(u8, u64)>,
    /// KEYED-24: the block host's `(site lane, arg)` (`win_host`'s block form: Filters allowed).
    blk: Option<(u8, u64)>,
    /// KEYED-24 (the condition cache on): the source covers every Filter op (`covered_ops`), so a covered window (a zone
    /// `AllMatch`, a cache hit's survivors as its selection) has no row a Filter drops; one with a selection is the window
    /// host's to take whole (`whole`).
    covered: bool,
    /// NEW-40: the runs signal's words (`covered` with a block host). None: no words.
    runs: Option<RunsWords>,
    /// DC-21: the record is a row of the keys (`row`): no row is skipped for a NULL.
    row: bool,
    mcx: Mcx<'g>,
}

impl CountEnd<'_, '_> {
    fn run(&self, env: &Env) -> Option<&'static dyn RunCarry> {
        self.run.filter(|_| env.applies(FRAME_CARRY))
    }

    /// `[cursor, end)` taken by `agg_distinct_window` over `host` (its rows counted out, the carry's run left unopened); a
    /// declined window leaves the builder where its rows are walked, a negative status leaves. None where no host applies.
    /// NEW-40 `runs`: the host writes the runs word into the state's scratch word, copied to the frame word after a taken
    /// window.
    fn window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, win: &Win, c: &Carry, l_wend: Block, host: Option<(u8, u64)>, runs: Option<(RunsWords, Value)>) -> Option<()> {
        let (Some((lane, arg)), Some(site), Some(_)) = (host, env.site, self.run(env)) else { return None };
        let st = crate::table::bypass::state(b, env, &self.t);
        let f = crate::table::bypass::host(b, "agg_distinct_window", &[Type::Ptr, Type::Ptr, Type::I64, Type::I64, Type::I64, Type::I64], gravity_abi::host_words!(gravity_kernels::ops::group::entries_distinct::agg_distinct_window(ctx, st, at, cursor, end, arg)));
        let (at, tl) = (b.iconst(Type::I64, (site << 8 | lane as usize) as i64), b.iconst(Type::I64, arg as i64));
        let r = b.call_host(f, &[env.ctx, st, at, win.cursor, win.end, tl], Some(Type::I64)).expect("agg_distinct_window returns a status");
        let x = env.x;
        crate::table::bypass::fail_on(b, env, &x, r);
        let (took, l_took, l_rows) = (b.icmp(IntCond::Eq, r, env.zero), b.create_block(&[]), b.create_block(&[]));
        b.cond_br(took, (l_took, &[]), (l_rows, &[]));
        b.switch_to(l_took);
        if let Some((w, ws)) = runs {
            let v = b.load(Type::I64, st, w.scratch as i32);
            b.store(ws, w.runs, v);
        }
        let mut params = c.params();
        let n = b.int_bin(IntBinOp::Sub, win.end, win.cursor);
        params[0] = b.int_bin(IntBinOp::Add, c.out, n);
        b.br(l_wend, &params);
        b.switch_to(l_rows);
        Some(())
    }
}

impl RunFold for CountEnd<'_, '_> {
    fn init<'e>(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>) -> PgVec<'e, Value> {
        PgVec::new_in(env.mcx)
    }

    fn step<'e>(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _row: &RowVals, accs: &[Value]) -> PgVec<'e, Value> {
        mcx::to_vec_in(accs, env.mcx)
    }

    fn close(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, img: &ImageWords, h: Value, _accs: &[Value]) {
        self.table.upsert(b, env, &self.t, img, h);
    }

    fn row_into(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _row: &RowVals, img: &ImageWords, h: Value) {
        self.table.upsert(b, env, &self.t, img, h);
    }
}

impl EndMech for CountEnd<'_, '_> {
    /// join.mult (H20): `m` copies of a row upsert its one key, as the row itself does.
    fn takes_mult(&self) -> bool {
        true
    }

    fn state_bytes(&self) -> u32 {
        self.table.state_bytes(&self.t) + if self.runs.is_some() { 8 } else { 0 }
    }

    fn open_words(&self, w: &mut OpenWords) {
        self.table.open_words(&self.t, w);
    }

    /// NEW-40: the host's scratch word filled from the worker's runs word.
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        self.table.open(b, env, &self.t);
        if let (Some(w), Some(ws)) = (self.runs, env.words) {
            let st = crate::table::bypass::state(b, env, &self.t);
            let v = b.load(Type::I64, ws, w.runs);
            b.store(st, w.scratch as i32, v);
        }
    }

    fn carried<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>) -> PgVec<'e, Value> {
        match self.run(env) {
            Some(run) => run.carried(b, env, &self.t, self),
            None => PgVec::new_in(env.mcx),
        }
    }

    /// A NULL argument skips the row (its carry passes through); any other is imaged, then upserted (or carried in the run).
    /// A row count (DC-21) skips none: its keys' NULLs are imaged into the flags words.
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        let one = b.iconst(Type::I64, 1);
        let out = b.int_bin(IntBinOp::Add, row.carry.out, one);
        let keys = self.rec.key_vals(b, env, row);
        let null = if self.row { None } else { keys[0].2 };
        let types: PgVec<'_, Type> = row.carry.accs.iter().map(|&v| b.value_type(v)).collect_in(self.mcx);
        let l_join = b.create_block(&types[..]);
        if let Some(n) = null {
            let l_do = b.create_block(&[]);
            b.cond_br(n, (l_join, &row.carry.accs), (l_do, &[]));
            b.switch_to(l_do);
        }
        // under the NULL skip: the datums read here do not dominate the join
        let accs = env.aside(|env| {
            let mut img = self.rec.image_of(b, env, &keys);
            crate::table::emit_group::derived_index(b, env, self.rec.node, row, self.src, &mut img);
            match self.run(env) {
                Some(run) => {
                    let win = crate::ops::agg::hashagg::flush_win(env);
                    run.row(b, env, &self.t, &win, row, &img, &row.carry.accs, self)
                }
                None => {
                    let h = emit_key_hash(b, &img.words);
                    self.row_into(b, env, row, &img, h);
                    mcx::to_vec_in(&row.carry.accs[..], env.mcx)
                }
            }
        });
        b.br(l_join, &accs);
        b.switch_to(l_join);
        Carry { out, accs: mcx::to_vec_in(b.block_params(l_join), env.mcx) }
    }

    /// KEYED-24: a whole 64-row block of a filtered window (the frame's block path: its Filters' masks, the steps after
    /// them keeping every row) taken by `agg_distinct_block`, the block's passing rows `m` upserted as the window host
    /// upserts a window's; the rows counted out, the carry's run left as it is (a dropped duplicate is the owner's). A
    /// declined block (a dense table.small, `DW_DECLINED`) leaves for the frame's walk of its rows; a negative status leaves.
    fn block_or_walk<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _w: &WordWin, base: Value, m: Mask, c: &Carry) -> Option<(Carry<'e>, Block)> {
        // no road of its own: road mask.fold applies to a plain fold's pipeline alone (applies.rs `FoldScan`), and the block
        // reaching here is mask.word's (the frame's block path); the host is the kind's own, as its window host is (`whole`)
        let (Some((lane, arg)), Some(site), Some(_)) = (self.blk, env.site, self.run(env)) else { return None };
        let st = crate::table::bypass::state(b, env, &self.t);
        let f = crate::table::bypass::host(b, "agg_distinct_block", &[Type::Ptr, Type::Ptr, Type::I64, Type::I64, Type::I64, Type::I64], gravity_abi::host_words!(gravity_kernels::ops::group::entries_distinct::agg_distinct_block(ctx, st, at, base, mask, arg)));
        let (at, tl) = (b.iconst(Type::I64, (site << 8 | lane as usize) as i64), b.iconst(Type::I64, arg as i64));
        let r = b.call_host(f, &[env.ctx, st, at, base, m.0, tl], Some(Type::I64)).expect("agg_distinct_block returns a status");
        let x = env.x;
        crate::table::bypass::fail_on(b, env, &x, r);
        let (took, l_took, l_no) = (b.icmp(IntCond::Eq, r, env.zero), b.create_block(&[]), b.create_block(&[]));
        b.cond_br(took, (l_took, &[]), (l_no, &[]));
        b.switch_to(l_took);
        let n = b.int_un(IntUnOp::Popcnt, m.0);
        Some((Carry { out: b.int_bin(IntBinOp::Add, c.out, n), accs: mcx::to_vec_in(&c.accs[..], env.mcx) }, l_no))
    }

    /// V4R-26 item 1: a window of the bypassed table taken whole by `agg_distinct_window` (its rows counted out, the carry's
    /// run left unopened); a declined window walks its rows, a negative status leaves.
    /// KEYED-24 (the condition cache on): a window whose Filters the source covers (`covered`) taken whole when its
    /// `WinConds::covered` holds, it has a selection (a condition-cache hit's survivors, a delete vector) and the table
    /// bypasses, by the window host over that selection: the pgrc host decodes every lane of a window with a selection, while a window with none
    /// (a zone AllMatch, a hit whose every row passes) leaves its late lanes for the body's `fill_late`, which runs after
    /// `whole`. Any other window walks.
    fn whole(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, win: &Win, c: &Carry, l_wend: Block) -> bool {
        if self.window(b, env, win, c, l_wend, self.win, None).is_some() {
            return true;
        }
        let (true, Some(cv), Some(nosel), Some(_), Some(_), Some(_)) = (self.covered, win.facts.covered, win.facts.nosel, self.blk, env.site, self.run(env)) else { return false };
        let (l_sel, l_cov, l_no) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        let cov = crate::frame::frame::all(b, env, &[cv]).unwrap_or(cv);
        b.cond_br(cov, (l_sel, &[]), (l_no, &[]));
        b.switch_to(l_sel);
        let nosel = crate::frame::frame::all(b, env, &[nosel]).unwrap_or(nosel);
        b.cond_br(nosel, (l_no, &[]), (l_cov, &[]));
        b.switch_to(l_cov);
        // a bypassed table's window: there each survivor is a partition append, which the host makes cheaper than the walk
        // at any selection; in table mode the host saves only the adjacent duplicates it drops, and over survivors without
        // runs the walk's inline probe is the faster one. NEW-40: so a table-mode window takes the host where the worker's
        // last measured window came in runs (the runs word), or to measure again once `W_LEFT` walked windows ran out
        let st = crate::table::bypass::state(b, env, &self.t);
        let bw = b.load(Type::I64, st, gravity_kernels::host_abi::AGG_BYPASS_WORD as i32);
        let (mut take, l_host) = (b.icmp(IntCond::Ne, bw, env.zero), b.create_block(&[]));
        let (mut host, mut copy) = (self.blk, None);
        if let (Some(w), Some(ws), Some((lane, arg))) = (self.runs, env.words, self.blk) {
            let (runs, left) = (b.load(Type::I64, ws, w.runs), b.load(Type::I64, ws, w.left));
            let (ran, due) = (b.icmp(IntCond::Ne, runs, env.zero), b.icmp(IntCond::Sle, left, env.zero));
            let either = b.int_bin(IntBinOp::Or, ran, due);
            take = b.int_bin(IntBinOp::Or, take, either);
            let l_walk = b.create_block(&[]);
            b.cond_br(take, (l_host, &[]), (l_walk, &[]));
            b.switch_to(l_walk);
            let next = b.int_bin(IntBinOp::Sub, left, env.one);
            b.store(ws, w.left, next);
            b.br(l_no, &[]);
            b.switch_to(l_host);
            let probe = b.iconst(Type::I64, RUNS_PROBE);
            b.store(ws, w.left, probe);
            (host, copy) = (Some((lane, arg | (w.scratch as u64) << 32)), Some((w, ws)));
        } else {
            b.cond_br(take, (l_host, &[]), (l_no, &[]));
            b.switch_to(l_host);
        }
        self.window(b, env, win, c, l_wend, host, copy);
        b.br(l_no, &[]);
        b.switch_to(l_no);
        true
    }

    fn window_end(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, c: &Carry) {
        if let Some(run) = self.run(env) {
            run.window_end(b, env, &self.t, &c.accs, self);
        }
        self.table.window_end(b, env, &self.t);
    }

    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) {
        self.table.finish(b, env, &self.t, x);
    }

    /// The owner phase dedups each partition of the key-only pairs and counts them (`distinct.total`); no legacy kernel
    /// fact words (keyed_hashagg's `pipeline_desc`).
    fn pipeline_desc(&self, _px: &Px<'_, '_>, _k: usize, d: Pipeline) -> Pipeline {
        Pipeline { words: None, ..d }.owner(crate::ops::group::distinct_total::owner_desc(self.t.layout))
    }
}

/// The emit's frame words: the cursor (0, then 1 past the one row) and the gather's state.
const W_CURSOR: u8 = 0;
const W_STATE: u8 = 1;

/// One aggregate's register block in the pair form's walk: a rider merges its record slot, a pair counts or folds the key.
enum Acc1<'a, 'p> {
    Rider(&'a crate::ops::agg::hashagg::Agg<'p>),
    Count,
    Fold(&'static dyn FoldClass, &'a AggDecl<'p>),
}

struct Emit<'g, 'p> {
    rec: GroupRec<'g, 'p>,
    form: Form,
    /// The records the gather reads: the pairs' accumulator (count form) or the hashed table's record.
    layout: RecordLayout,
    words: [i32; 2],
    node: i32,
    /// An embedded inner's table at a later region of its owner's call (two SubPlans in one select list, the count(DISTINCT)
    /// one past the first: rerun/sets.rs `run_region`): gathered there, fresh at every call (`pairs_total_at` /
    /// `agg_gather_at`), the owner's resume word untouched; any region of a spooled drain's tail (its own list). None:
    /// consumed region 0, as before.
    at: Option<u64>,
    mcx: Mcx<'g>,
}

fn make_emit<'g, 'p>(px: &mut Px<'g, 'p>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let (g, node) = group(px, k, true);
    if crate::ops::group::distinct_multi::multi(g) {
        return crate::ops::group::distinct_multi::make_emit(px, k, g);
    }
    let form = form(g, node);
    let rec = if form == Form::Count { count_rec(px, g) } else { GroupRec::new(px, g) };
    let layout = if form == Form::Count { crate::ops::group::distinct_total::owner_desc(rec.layout).acc } else { rec.layout };
    let word = |px: &mut Px<'_, '_>, i: u8| px.frame_word(WordKey::Open { face: Face::Keyed, k, j: u64::MAX, i }) as i32 * 8;
    let words = [word(px, W_CURSOR), word(px, W_STATE)];
    // a spooled drain's tail (a Hash Right / Full Join's unmatched rows) binds its own list: its region 0 is gathered by index too
    let tail = px.tails.iter().any(|t| t.ident == px.ident(k));
    // `run_region` gives the running call's index: past a LAZY hashed SubPlan's set (its slot leaves the list after lowering) one
    // lower per set; a tail's list refuses past one
    let at = (k >= px.gx.pipelines().len()).then(|| crate::rerun::sets::run_region(px, k, g.slot)).flatten().filter(|&i| i != 0 || tail).map(|i| i as u64);
    mcx::pg_box_dyn!(px.mcx(), Emit { rec, form, layout, words, node, at, mcx: px.mcx() } => dyn SourceMech + 'g)
}

impl<'g, 'p> Emit<'g, 'p> {
    /// The gather's state filed in the state word; a null state refuses (the open's status).
    fn filed(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, st: Value) -> Value {
        let sti = b.conv(ConvOp::PtrToInt, Type::I64, st);
        env.store_word(b, self.words[W_STATE as usize], sti);
        let refused = b.icmp(IntCond::Eq, sti, env.zero);
        let unsup = b.iconst(Type::I64, unsupported(UNSUPPORTED_SCRATCH));
        b.select(refused, unsup, env.zero)
    }

    /// Every aggregate's final into a stack area, 16 bytes each (the datum, then its valid word), as the row's cells by aggno.
    fn finals<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, st: Value) -> AggRow<'e> {
        let aggs = &self.rec.g.aggs;
        let area = {
            let slot = b.declare_stack_slot(16 * aggs.len() as u32, 8);
            b.stack_addr(slot)
        };
        let naggno = aggs.iter().map(|d| d.aggno.max(0) as usize + 1).max().unwrap_or(0);
        let mut cells = mcx::pg_vec![in env.mcx; AggCell::word(0, CellValid::Always); naggno];
        let ptrs = b.load(Type::Ptr, st, ES_PTRS);
        let fins = match self.form {
            Form::Count => {
                let rec = b.load(Type::Ptr, ptrs, 0);
                let total = b.load(Type::I64, rec, self.layout.slot_off(ACC_PAIRS as usize) as i32);
                let f = b.bconst(false);
                mcx::pg_vec![in self.mcx; (total, f); aggs.len()]
            }
            Form::Pairs => {
                let kept = b.load(Type::I64, st, ES_KEPT);
                self.walk(b, env, ptrs, kept)
            }
        };
        for (i, (d, (v, null))) in aggs.iter().zip(fins).enumerate() {
            let off = 16 * i as u32;
            b.store(area, off as i32, v);
            let t = b.bconst(true);
            let valid = b.int_bin(IntBinOp::Xor, null, t);
            let valid = b.conv(ConvOp::Zext, Type::I64, valid);
            b.store(area, off as i32 + 8, valid);
            cells[d.aggno as usize] = AggCell::word(off, CellValid::NonZeroAt(off + 8));
        }
        AggRow { rec: area, cells }
    }

    /// The pair form's walk over the `kept` records at `ptrs`: `(final, null)` per aggregate in the group's order.
    fn walk(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, ptrs: Value, kept: Value) -> PgVec<'g, (Value, Value)> {
        let mut blocks: PgVec<'_, (Acc1<'_, 'p>, usize, usize)> = mcx::PgVec::new_in(self.mcx);
        let mut accs: PgVec<'_, Value> = mcx::PgVec::new_in(self.mcx);
        let mut riders = self.rec.aggs.iter();
        for d in self.rec.g.aggs.iter() {
            let (a, words) = match d.final_ {
                FinalForm::PairCount => (Acc1::Count, 1),
                FinalForm::PairFold => {
                    let c = crate::ops::agg::trans::plain::class_of(&self.node, d, self.mcx);
                    (Acc1::Fold(c, d), c.shape(d).words as usize)
                }
                _ => {
                    let r = riders.next().unwrap_or_else(|| bug(self.node, "a rider missing from the group's record"));
                    (Acc1::Rider(r), r.shape.words as usize)
                }
            };
            let mut regs = mcx::pg_vec![in self.mcx; env.zero; words];
            match &a {
                Acc1::Rider(r) => r.class.init(b, env, &r.d, Acc::Regs(&mut regs)),
                Acc1::Fold(c, d) => c.init(b, env, d, Acc::Regs(&mut regs)),
                Acc1::Count => {}
            }
            blocks.push_in((a, accs.len(), words));
            accs.extend_in(regs);
        }
        let types: PgVec<'_, Type> = accs.iter().map(|&v| b.value_type(v)).collect_in(self.mcx);
        let mut params = mcx::pg_vec![in self.mcx; Type::I64];
        params.extend_in(types.iter().copied());
        let (h, body) = cancel::header(b, env.ctx, env.l_cancel, &params[..], env.mcx);
        let l_done = b.create_block(&types[..]);
        let mut first = mcx::pg_vec![in self.mcx; env.zero];
        first.extend_in(accs.iter().copied());
        b.br(h, &first[..]);
        b.switch_to(body);
        let i = b.block_params(body)[0];
        let mut accs: PgVec<'_, Value> = mcx::to_vec_in(&b.block_params(body)[1..], self.mcx);
        let more = b.icmp(IntCond::Slt, i, kept);
        let l_step = b.create_block(&[]);
        b.cond_br(more, (l_step, &[]), (l_done, &accs[..]));
        b.switch_to(l_step);
        let at = b.ptr_add(ptrs, i, 8, 0);
        let rec = b.load(Type::Ptr, at, 0);
        for (a, s, w) in blocks.iter() {
            if let Acc1::Rider(r) = a {
                r.class.merge(b, env, &r.d, Acc::Regs(&mut accs[*s..*s + *w]), self.rec.slot(rec, r.d.slot));
            }
        }
        self.pairs(b, env, rec, &blocks[..], &mut accs[..], &types[..]);
        let one = b.iconst(Type::I64, 1);
        let i2 = b.int_bin(IntBinOp::Add, i, one);
        let mut next = mcx::pg_vec![in self.mcx; i2];
        next.extend_in(accs.iter().copied());
        b.br(h, &next[..]);
        b.switch_to(l_done);
        let mut accs: PgVec<'_, Value> = mcx::to_vec_in(b.block_params(l_done), self.mcx);
        let f = b.bconst(false);
        blocks
            .iter()
            .map(|(a, s, w)| match a {
                Acc1::Count => (accs[*s], f),
                Acc1::Fold(c, d) => c.finish(b, env, d, Acc::Regs(&mut accs[*s..*s + *w])),
                Acc1::Rider(r) => r.class.finish(b, env, &r.d, Acc::Regs(&mut accs[*s..*s + *w])),
            })
            .collect_in(self.mcx)
    }

    /// Record `rec`'s key counted / folded once by each pair, unless it is the NULL argument's record.
    fn pairs(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, rec: Value, blocks: &[(Acc1<'_, 'p>, usize, usize)], accs: &mut [Value], types: &[Type]) {
        if !blocks.iter().any(|(a, _, _)| !matches!(a, Acc1::Rider(_))) {
            return;
        }
        let l_join: Option<Block> = self.rec.flags_word().map(|fw| {
            let flags = b.load(Type::I64, rec, 8 * fw as i32);
            let one = b.iconst(Type::I64, 1);
            let bit = b.int_bin(IntBinOp::And, flags, one);
            let null = b.icmp(IntCond::Ne, bit, env.zero);
            let (l_do, l_join) = (b.create_block(&[]), b.create_block(types));
            b.cond_br(null, (l_join, accs), (l_do, &[]));
            b.switch_to(l_do);
            l_join
        });
        let key = b.load(Type::I64, rec, 0);
        let kd = &self.rec.g.keys[0];
        let (cell, key) = match crate::rep::hosted::order::word_key_len(&self.rec.tfs[0]) {
            // D2474: a macaddr / macaddr8 argument's record word is its order word, never its datum: a fold reads its bytes
            // rendered back from the word (emit_key.rs `KeyOut::Bytes`)
            Some(len) => {
                let p = crate::rep::hosted::order::bytes_of_word(b, key, len);
                let life = CellLife { until: Until::Row, home: CellHome::FrameSlot };
                (Cell { rep: Rep::FixedPtr { len }, typid: kd.tf, num: NumTypmod::NONE, life, at: At::Slot(0), null: Nullable::Never }, p)
            }
            None => (Cell::of_record(Rep::Word { bytes: 8 }, kd.tf, NumTypmod::NONE, self.rec.home, 0, Nullable::Never), key),
        };
        for (a, s, w) in blocks.iter() {
            match a {
                Acc1::Count => {
                    let one = b.iconst(Type::I64, 1);
                    accs[*s] = b.int_bin(IntBinOp::Add, accs[*s], one);
                }
                Acc1::Fold(c, d) => c.step(b, env, d, Acc::Regs(&mut accs[*s..*s + *w]), &[CellRef { cell: &cell, val: key, null: None }]),
                Acc1::Rider(_) => {}
            }
        }
        if let Some(l) = l_join {
            b.br(l, accs);
            b.switch_to(l);
            accs.copy_from_slice(b.block_params(l));
        }
    }
}

impl<'g, 'p> SourceMech for Emit<'g, 'p> {
    fn open_words(&self, w: &mut OpenWords) {
        for (i, what) in ["distinct cursor", "distinct state"].into_iter().enumerate() {
            w.declare(OpenWord::open(what, Base::Words, self.words[i], Type::I64, 0));
        }
    }

    /// The fresh call's gather (the kernel's fresh test is the resume word, so it reads NONE around the call): the pairs'
    /// one accumulator record (count form) or every record of the table (pair form); a null state refuses.
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        let desc = crate::table::emit_group::desc(b, &self.layout, &[], 0);
        let mem = HostFlags { touches_memory: true, ..HostFlags::default() };
        if let Some(at) = self.at {
            let st = match self.form {
                Form::Count => {
                    let f = b.declare_host_once("pairs_total_at", &[Type::Ptr, Type::Ptr, Type::I64], Some(Type::Ptr), pairs_total_at as *const () as usize, mem);
                    let at = b.iconst(Type::I64, at as i64);
                    b.call_host(f, &[env.ctx, desc, at], Some(Type::Ptr))
                }
                Form::Pairs => {
                    let f = b.declare_host_once("agg_gather_at", &[Type::Ptr, Type::Ptr, Type::I64, Type::Ptr], Some(Type::Ptr), agg_gather_at as *const () as usize, mem);
                    let (at, scratch) = (b.iconst(Type::I64, (at | GATHER_FRESH) as i64), b.iconst(Type::Ptr, 0));
                    b.call_host(f, &[env.ctx, desc, at, scratch], Some(Type::Ptr))
                }
            };
            return self.filed(b, env, st.expect("the gather returns its state"));
        }
        let gather = match self.form {
            Form::Count => crate::ops::group::distinct_total::gather(b),
            Form::Pairs => b.declare_host_once("agg_gather", &[Type::Ptr, Type::Ptr], Some(Type::Ptr), agg_gather as *const () as usize, mem),
        };
        let none = b.iconst(Type::I64, RESUME_NONE);
        b.store(env.ctx, hdr::RESUME as i32, none);
        let st = b.call_host(gather, &[env.ctx, desc], Some(Type::Ptr)).expect("the gather returns its state");
        let open = b.iconst(Type::I64, RESUME_OPEN);
        b.store(env.ctx, hdr::RESUME as i32, open);
        self.filed(b, env, st)
    }

    /// `[cursor, 1)`: the one row, then the end.
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let cursor = env.load_word(b, self.words[W_CURSOR as usize]);
        let past = b.icmp(IntCond::Sge, cursor, env.one);
        let l_win = b.create_block(&[]);
        b.cond_br(past, (l_eof, &[env.zero]), (l_win, &[]));
        b.switch_to(l_win);
        Win { cursor, end: env.one, facts: WinConds::default() }
    }

    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        let st = env.load_word(b, self.words[W_STATE as usize]);
        let st = b.conv(ConvOp::IntToPtr, Type::Ptr, st);
        env.aggs = Some(self.finals(b, env, st));
        RowVals { row: Row { cells: PgVec::new_in(env.mcx) }, vals: PgVec::new_in(env.mcx), cols: PgVec::new_in(env.mcx), carry, next, at: j }
    }

    fn write_back(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, cur: Value, _out: Value) {
        env.store_word(b, self.words[W_CURSOR as usize], cur);
    }
}

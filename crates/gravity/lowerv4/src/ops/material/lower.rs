//! Kind `nest.spool` (the Material spool walked in IR; L3) (cards/M8.md, M8-changes.md §1). Owner M8-NEST. S1, the pull form
//! (the B1s design): a plan Material that one nest (or nested SubPlan) inner reads is PostgreSQL's
//! Material exactly. Its child, one scan of filters and projections reading no executor parameter (pipes.rs `spool_pull`),
//! is made again inside the inner over a site of its own (nest/spool_child.rs), numbered where exec admit pairs it with
//! the fill pipeline's own source (nest/spool_site.rs `walk_site`; the reader is the innermost nest inner, spool_readers.rs
//! D1919); the fill pipeline itself reads nothing (graph.rs `source_kind` maps its source here: `Empty`)
//! and leaves at its open (`Fused`). Per re-open the inner's position is 0 (a per-open frame word). A window is the records
//! from the position to the count; at the count, unless the child has ended, the child reads on from where it stopped (its
//! site's cursor, opened once at the first read) until one row passes its ops, which is appended (nest/spool_rec.rs) and
//! is the window. So a row is made only when a reader first reaches it and never twice: a SEMI / ANTI partial read leaves
//! the rest unread until a later re-open walks past it, and an empty outer never runs the child. The count, the child's end
//! and its open are per-execution words (zeroed at bind, as the spool object is laid at every prepare); the reader is
//! site-less (its position is its own word, nest/inner.rs `set_cursor`). The nest's pipeline is serial (nest.loop's
//! `StepMech::serial`, the spool's one filler). Refused by name: a child reading an executor parameter (D1107), a child
//! other than one scan of filters and projections (D1108), a Material another than one nest inner reads (D1109), a pulled
//! child over pgrcolumnar (D1484), a child no site number pairs with its own source (D1633: nest/spool_site.rs).
//! G2(e), S1u: a child that is a settled union the eager form refuses is pulled arm after arm (`union_reader`).
//! D3498: a Result's one row the eager form refuses (rerun/spool/form.rs `result_child`) is pulled with no site, its
//! position a word of the reader's own.
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::graph::Bound;
use gravity_decompose::{Op, Source};
use mcx::{CollectIn, Mcx, PgBox, PgRc, PgVec};
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{Block, IntBinOp, IntCond, Type, Value};
use gravity_program::{NumTypmod, Pipeline, Rep, Until};
use types_nodes::list::NodeList;

use crate::rerun::spool::child::Child;
use crate::rerun::spool::rec::Rec;
use crate::cell::{At, Cell, CellHome, CellLife, Nullable, Row};
use crate::face::end::EndMech;
use crate::face::kinds::{Face, KindId};
use crate::face::source::{SourceMech, WinConds};
use crate::face::step::StepMech;
use crate::frame::env::Env;
use crate::frame::frame::{EndMaker, Next, SourceMaker, StepMaker, Win};
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{Px, WordKey, DRAFT_SITE};
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::pipes::{spool_fill, LOUD_CHILD, MANY_READERS, PGRC_CHILD};
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::ops::Table;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::NestSpool, &SourceMaker(make_source)), (KindId::NestSpool, &StepMaker(make_step)), (KindId::NestSpool, &EndMaker(make_end))], ..crate::ops::NONE };

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

fn bug(node: i32, payload: &'static str) -> ! {
    crate::refuse!(node, Reason::Bug { payload })
}

/// The fill pipeline `k`'s reader and form (pull: true), else the Material refuses by the reason's name.
fn form(px: &Px<'_, '_>, k: usize) -> (usize, bool) {
    match spool_fill(px.gx, k) {
        Ok(f) => f,
        Err("") => bug(px.gx.pipelines()[k].node_id, "a nest.spool fill that is no plan Material's"),
        Err(why) => shape(px.gx.pipelines()[k].node_id, why),
    }
}

/// The Spool op: the fill pipeline's row as it is (the pipeline leaves at its open; its inner reader appends).
struct Pass;

impl StepMech for Pass {
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        next(b, env, row)
    }
}

fn make_step<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    if matches!(px.gx.pipelines()[k].ops.get(j), Some(Op::Append(_))) {
        return crate::rerun::union_arms::step(px, k, j); // S2a: a settled union's Feed / Union (source/append.rs hands them here)
    }
    if !matches!(px.gx.pipelines()[k].ops.get(j), Some(Op::Join(JoinDesc::Spool))) {
        bug(px.gx.pipelines()[k].node_id, "a nest.spool step over another op");
    }
    form(px, k);
    mcx::pg_box_dyn!(px.mcx(), Pass => dyn StepMech + 'g)
}

/// The fill pipeline's end: it reads nothing, so its function leaves at the open (the inner pulls its rows).
struct Fused;

impl EndMech for Fused {
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        b.br(env.x.l_done, &[]);
        let dead = b.create_block(&[]);
        b.switch_to(dead);
    }

    fn lower<'e>(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        row.carry.copy_in(env.mcx)
    }
}

fn make_end<'g, 'p>(px: &mut Px<'g, 'p>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    if crate::rerun::union_arms::feed_of(&px.gx.pipelines()[k]).is_some() {
        return crate::rerun::union_arms::end(px, k);
    }
    if form(px, k).1 {
        return mcx::pg_box_dyn!(px.mcx(), Fused => dyn EndMech + 'g);
    }
    // errorhold (D1108 row 2): a fill that may raise runs ahead of PostgreSQL's first fetch under the recheck licence, or
    // (`DECISIONS.md` 4 (D) b) where no plan-order run re-checks it, unchecked (`Px::ahead_unchecked`, tagged)
    let raises = gravity_decompose::effects::Effects::RAISES;
    let mut ahead = None;
    if crate::rerun::spool::form::eager_licensed(px.gx, k) {
        ahead = px.run_ahead(k, raises).or_else(|| px.ahead_unchecked(k, raises));
        if ahead.is_none() {
            shape(px.gx.pipelines()[k].node_id, LOUD_CHILD);
        }
    }
    // D3373: D1107's eager refill of a child that may raise runs ahead of PostgreSQL's pulls only under the unchecked licence
    if crate::lifetime::material::refilled_ahead(px.gx, k) {
        let refilled = px.ahead_unchecked(k, raises);
        if refilled.is_none() {
            shape(px.gx.pipelines()[k].node_id, crate::rerun::spool::form::REFILL_CHILD);
        }
        ahead = ahead.or(refilled);
    }
    let p = &px.gx.pipelines()[k];
    let Some(Op::Project(cols)) = p.ops.iter().rev().nth(1) else { shape(p.node_id, LOUD_CHILD) };
    // D1108's held form (rerun/spool/form.rs `held`): the fill's error deferred by its merge (exec ops/sort/held.rs stamps
    // the Material slot's region RUN_HELD) and raised by the reader at the record the fill stopped at (`Reader::held`)
    if let Some(held) = crate::rerun::spool::form::held(px.gx, k).filter(|_| !px.held_sort.contains(&k)) {
        ahead = ahead.or(Some(held));
        px.held_sort.push(k); // alloc: Program
        // D1108: its reader nest inside an embedded inner, the fill runs in pipeline order, its feeders and it serially on
        // the leader, PostgreSQL's heap order (form.rs `held_serial`)
        for q in crate::rerun::spool::form::held_serial(px.gx, k) {
            if !px.serial.contains(&q) {
                px.serial.push(q); // alloc: Program
            }
        }
    }
    // construct C2: an eager arm whose own test proves the fill quiet holds the unobservable licence
    // (lifetime/material.rs `quiet_fill_licence`)
    ahead = ahead.or_else(|| crate::lifetime::material::quiet_fill_licence(px.gx, k));
    // construct C2 (b): the fill runs ahead of PostgreSQL's first fetch under that licence (the held form's held_error, the
    // quiet arm's unobservable one; run_ahead / ahead_unchecked hold theirs already): the run-ahead tracer reads it
    if let Some(l) = &ahead {
        px.hold(k, l);
    }
    let (rec, _) = Rec::declare(px, k, k, p.node_id, cols, 0);
    // D1107's eager refill (rerun/spool/form.rs `refilled`): a rerun set's member runs at every re-open of its owner
    let refill = px.gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(k as u16)));
    mcx::pg_box_dyn!(px.mcx(), Fill { rec, refill, fresh: None, ahead } => dyn EndMech + 'g)
}

/// S2a (nest/union_arms.rs): a settled union's feed end over the union's record; `refill`: it empties the record at its
/// open (S2r: the first feed of a union a nest's rerun set refills).
pub(crate) fn fill<'g>(mcx: Mcx<'g>, rec: Rec<'g>, refill: bool) -> PgBox<'g, dyn EndMech + 'g> {
    mcx::pg_box_dyn!(mcx, Fill { rec, refill, fresh: None, ahead: None } => dyn EndMech + 'g)
}

/// D3614: a feed of a held union whose rerun sets prune its arms at every run (rerun/union_arms.rs `fresh`): the first
/// feed to open in a run empties the record and sets frame word `w`; a pruned feed never opens (D2105), so no fixed feed
/// can (`eager_fresh` empties it when none opened).
pub(crate) fn fill_fresh<'g>(mcx: Mcx<'g>, rec: Rec<'g>, w: i32) -> PgBox<'g, dyn EndMech + 'g> {
    mcx::pg_box_dyn!(mcx, Fill { rec, refill: false, fresh: Some(w), ahead: None } => dyn EndMech + 'g)
}

/// S2a: the eager reader of a settled union's record, emit pipeline `k`'s source (its position a per-open frame word).
pub(crate) fn eager<'g>(px: &mut Px<'g, '_>, k: usize, rec: Rec<'g>, node: i32) -> PgBox<'g, dyn SourceMech + 'g> {
    let w_j = px.frame_word(WordKey::Open { face: Face::Nest, k, j: u64::MAX, i: 2 }) as i32 * 8;
    mcx::pg_box_dyn!(px.mcx(), Reader { child: None, arms: None, rec, st: 0, w_j, node, held: None, fresh: None, cap: None } => dyn SourceMech + 'g)
}

/// D3614: `eager` over a held union's record whose feeds empty it by frame word `w` (`fill_fresh`): its open empties the
/// record when no feed opened in this run (every arm pruned) and clears `w` for the next run's first feed.
pub(crate) fn eager_fresh<'g>(px: &mut Px<'g, '_>, k: usize, rec: Rec<'g>, node: i32, w: i32) -> PgBox<'g, dyn SourceMech + 'g> {
    let w_j = px.frame_word(WordKey::Open { face: Face::Nest, k, j: u64::MAX, i: 2 }) as i32 * 8;
    mcx::pg_box_dyn!(px.mcx(), Reader { child: None, arms: None, rec, st: 0, w_j, node, held: None, fresh: Some(w), cap: None } => dyn SourceMech + 'g)
}

/// D3614: `rec` emptied unless frame word `w` is set (`set`: then set to 1, a feed's open; else cleared, the emit's).
fn fresh_open(b: &mut FunctionBuilder<'_>, env: &mut Env, rec: &Rec, w: i32, set: bool) {
    let (l_empty, l_on) = (b.create_block(&[]), b.create_block(&[]));
    let f = env.load_word(b, w);
    let none = b.icmp(IntCond::Eq, f, env.zero);
    b.cond_br(none, (l_empty, &[]), (l_on, &[]));
    b.switch_to(l_empty);
    rec.empty(b, env);
    b.br(l_on, &[]);
    b.switch_to(l_on);
    env.store_word(b, w, if set { env.one } else { env.zero });
}

/// The eager form's end: each row of the Material's child appended (its pipeline serial: the spool's one filler). A refill
/// (D1107: the fill is a rerun set's member) empties the record at its open, as ExecReScanMaterial clears its tuplestore;
/// D3614: a fresh word's feed, when it is the run's first to open (`fill_fresh`).
struct Fill<'g> {
    rec: Rec<'g>,
    refill: bool,
    fresh: Option<i32>,
    /// Construct C2: the licence the fill runs ahead of PostgreSQL's first fetch under (the eager form's recheck or
    /// unchecked licence, D1108 row 2; D1107's eager refill's unchecked one, D3373; D1108's held form's held error, its
    /// error raised at the record the reader reaches; a quiet arm's unobservable one, lifetime/material.rs
    /// `quiet_fill_licence`); `None`: it fills at PG's point.
    /// Held, not read: the proof the fill's run ahead was granted.
    #[allow(dead_code)]
    ahead: Option<gravity_decompose::effects::Licence>,
}

impl EndMech for Fill<'_> {
    fn pipeline_desc(&self, _px: &Px<'_, '_>, _k: usize, d: Pipeline) -> Pipeline {
        match gravity_program::step_serial(&gravity_program::StepShape::MaterialFill) {
            Some(reason) => Pipeline { parallel: gravity_program::Parallel::Ineligible { site: d.driving_site, reason }, ..d },
            None => d,
        }
    }

    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        if self.refill {
            self.rec.empty(b, env);
        }
        if let Some(w) = self.fresh {
            fresh_open(b, env, &self.rec, w, true);
        }
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        self.rec.append(b, env, row);
        row.carry.copy_in(env.mcx)
    }
}

/// A source kind nest.spool makes: the inner's reader over a Material's breaker, or the pulled fill pipeline's own.
fn make_source<'g, 'p>(px: &mut Px<'g, 'p>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    match px.gx.pipelines()[k].source {
        Source::Breaker { .. } if crate::rerun::union_arms::union_of(&px.gx.pipelines()[k]).is_some() => crate::rerun::union_arms::source(px, k),
        Source::Breaker { slot } => make_reader(px, k, slot),
        _ => {
            form(px, k);
            empty(px, k)
        }
    }
}

/// The pulled fill pipeline's source: it reads nothing. Its row, in dead code only, carries the relation's columns as
/// NULL cells so its ops still lower.
struct Empty<'g> {
    cols: PgVec<'g, (u32, Rep)>,
}

fn empty<'g>(px: &Px<'g, '_>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    // D3498: a pulled Result child's row has no cells
    if matches!(p.source, Source::Const) {
        return mcx::pg_box_dyn!(px.mcx(), Empty { cols: PgVec::new_in(px.mcx()) } => dyn SourceMech + 'g);
    }
    let Some((_, relid, _)) = p.source.relation() else { bug(p.node_id, "a pulled Material child without its relation") };
    let natts = gx.facts.rel(relid).map_or(0, |r| r.natts.max(0) as usize);
    let cols = (1..=natts as i16).map(|a| match gx.facts.att(relid, a) {
        Some(f) if f.byval => (f.typid, Rep::Word { bytes: f.typlen.clamp(1, 8) as u8 }),
        Some(f) if f.typlen > 0 => (f.typid, Rep::FixedPtr { len: f.typlen as u16 }),
        Some(f) => (f.typid, Rep::VarlenaPtr),
        None => (types_core::INT8OID, Rep::Word { bytes: 8 }),
    }).collect_in(px.mcx());
    mcx::pg_box_dyn!(px.mcx(), Empty { cols } => dyn SourceMech + 'g)
}

/// G2(e), S1u: a pulled union's emit source (rerun/union_arms.rs `source`): it reads nothing (the Material's reader pulls
/// the arms). Its row, in dead code only, carries the union's columns (first feed `f0`'s projection) as NULL cells so the
/// emit's ops still lower.
pub(crate) fn union_empty<'g>(px: &Px<'g, '_>, k: usize, f0: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let gx = px.gx;
    let p = &gx.pipelines()[f0];
    let Some(Op::Project(cols)) = p.ops.iter().rev().nth(1) else { shape(gx.pipelines()[k].node_id, LOUD_CHILD) };
    let rep = |t: u32| gravity_decompose::catalog::typefacts::facts_of(gx.facts, t).map_or(Rep::Word { bytes: 8 }, |f| Rep::of_storage(f.typlen, f.byval));
    let ty = |n: types_nodes::Node<'_>| gravity_decompose::catalog::expr_type::type_of(n).unwrap_or_else(|| gravity_decompose::catalog::expr_type::result_type(n));
    let cols = cols.iter().map(|n| n.as_target_entry().map_or(types_core::INT8OID, |te| ty(te.expr))).map(|t| (t, rep(t))).collect_in(px.mcx());
    mcx::pg_box_dyn!(px.mcx(), Empty { cols } => dyn SourceMech + 'g)
}

impl<'g> SourceMech for Empty<'g> {
    fn open(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        env.zero
    }

    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        b.br(l_eof, &[env.zero]);
        let dead = b.create_block(&[]);
        b.switch_to(dead);
        Win { cursor: env.zero, end: env.zero, facts: WinConds::default() }
    }

    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        let life = CellLife { until: Until::Row, home: CellHome::FrameSlot };
        let cells = self.cols.iter().enumerate().map(|(c, &(typid, rep))| Cell { rep, typid, num: NumTypmod::NONE, life, at: At::Slot(c as u16), null: Nullable::Maybe }).collect_in(env.mcx);
        let t = b.bconst(true);
        let vals = self.cols.iter().map(|_| (env.zero, Some(t))).collect_in(env.mcx);
        RowVals { row: Row { cells }, cols: (0..self.cols.len() as u16).map(Some).collect_in(env.mcx), vals, carry, next, at: j }
    }
}

/// The inner's reader: the pull form's child (none in the eager form: its fill pipeline ran first), the record, the
/// per-execution words after the count (`st`: the child's end, then its open) and the position.
struct Reader<'g> {
    child: Option<Child<'g>>,
    arms: Option<Pulled<'g>>,
    rec: Rec<'g>,
    st: u16,
    w_j: i32,
    node: i32,
    /// D1108's held form (eager only): the Material slot's consumed index; at the record count the reader asks whether
    /// the fill's error was held there (kernels `m11_run_held`) and raises it, else the spool ends
    held: Option<u64>,
    /// D3614: the held union's fresh word (`eager_fresh`)
    fresh: Option<i32>,
    /// D4204: a pulled LOCK-FILL child's LIMIT count (`PulledLimit`): met, the child ends before it reads again
    cap: Option<i64>,
}

/// The Material's inner reader `ik` over `slot`: the fill pipeline found by the slot's life; in the pull form its child
/// made here, in the eager form the record its end declared.
fn make_reader<'g, 'p>(px: &mut Px<'g, 'p>, ik: usize, slot: u8) -> PgBox<'g, dyn SourceMech + 'g> {
    let gx = px.gx;
    let ip = &gx.pipelines()[ik];
    let node = ip.node_id;
    let d = &ip.desc;
    let life = d.consumes.iter().position(|&s| s == slot).and_then(|j| d.consumes_from.get(j).copied());
    let Some(kf) = life.map(|l| l.0 as usize) else { shape(node, MANY_READERS) };
    let (reader, pull) = form(px, kf);
    // rowsecurity#343: or a later arm's nest copy over the record the first arm's fill declared (readers.rs `copies`)
    if reader != ik && (pull || !crate::rerun::spool::readers::copies(gx, kf).is_some_and(|c| c.contains(&ik))) {
        shape(node, MANY_READERS);
    }
    let fp = &gx.pipelines()[kf];
    let body = &fp.ops[..fp.ops.len() - 1];
    let Some(Op::Project(cols)) = body.last() else { shape(node, LOUD_CHILD) };
    let w_j = px.frame_word(WordKey::Open { face: Face::Nest, k: ik, j: u64::MAX, i: 2 }) as i32 * 8;
    if pull && crate::rerun::union_arms::union_of(fp).is_some() {
        return union_reader(px, (ik, kf), cols, w_j, node);
    }
    if !pull {
        let rec = Rec::find(px, kf, node, cols);
        // D1108's held form: the Material slot's index among the nest pipeline's consumed regions, whose run the fill's
        // merge stamped held when the fill raised
        let held = px.held_sort.contains(&kf).then(|| {
            let o = gx.pipelines().inners().get(ik.wrapping_sub(gx.pipelines().len())).map(|x| x.0 .0);
            match o.and_then(|o| px.consumed(o, slot)) {
                Some(i) => i as u64,
                None => crate::refuse!(node, Reason::Bug { payload: "a held Material read by a nest whose pipeline does not consume its slot" }),
            }
        });
        return mcx::pg_box_dyn!(px.mcx(), Reader { child: None, arms: None, rec, st: 0, w_j, node, held, fresh: None, cap: None } => dyn SourceMech + 'g);
    }
    let kind = crate::graph::source_kind_of(&fp.source, fp.node_id);
    let ident = px.ident(ik);
    let protos = px.protos.len();
    // D3498: a Result child (rerun/spool/form.rs `result_child`) scans no site: its one row's position is the reader's own
    // word (`Env::pos`), written RESUME_OPEN at the child's open, as a pulled union keeps a Result arm's (rerun/inner_arms.rs);
    // its one-time gates are checked at the child's open, never steps, as a pulled union's gated arm's (inner_arms_gate.rs)
    let one = matches!(fp.source, Source::Const);
    // D4204: a LOCK-FILL child (rerun/spool/form.rs `lock_fill_pulls`): its lock made here, its LIMIT a `PulledLimit`
    let locked = crate::rerun::spool::form::lock_fill_pulled(gx, kf);
    let st_at = PgRc::new_in(std::cell::Cell::new(0u16), px.mcx());
    let (mut src, steps, gates) = px.making(kf, ident, |px| {
        let src = (crate::frame::iface::kind_impl::<SourceMaker>(&fp.node_id, kind).0)(px, kf);
        let gates = if one { crate::rerun::inner_arms_gate::compile(px, fp, kf) } else { Vec::new() };
        (src, child_steps(px, kf, body.len(), locked.then(|| st_at.clone())), gates)
    });
    src.in_order();
    let at = if one {
        Ok(px.frame_word(WordKey::Open { face: Face::Nest, k: ik, j: u64::MAX, i: 3 }) as i32 * 8)
    } else {
        let draft = px.making(kf, ident, |px| {
            let mut d = px.take_site(kf).or_else(|| src.site_desc(px.arena));
            if let Some(d) = d.as_mut() {
                src.seal(px, kf, d);
            }
            d
        });
        let Some(draft) = draft else { shape(node, LOUD_CHILD) };
        // numbered here, where exec admit pairs it with the fill pipeline's own source (nest/spool_site.rs)
        let site = crate::rerun::spool::site::walk_site(px, ik, kf, draft);
        px.set_pipeline_site(kf, Some(site)); // a row mark's recheck finds the marked relation's producer here (marks.rs)
        Err(site)
    };
    let site = at.err().map_or(-1, |s| s as i16);
    for p in px.protos[protos..].iter_mut().filter(|p| p.site == DRAFT_SITE) {
        p.site = site;
    }
    if src.covers() != 0 {
        shape(node, PGRC_CHILD);
    }
    if src.once() && !one || steps.iter().any(|s| s.expand().is_some()) {
        shape(node, LOUD_CHILD);
    }
    let mut words = OpenWords::new_in(px.mcx());
    if let Ok(o) = at {
        words.declare(OpenWord::open("nest.spool result child position", Base::Words, o, Type::I64, gravity_kernels::util::RESUME_OPEN));
    }
    src.open_words(&mut words);
    steps.iter().for_each(|s| s.open_words(&mut words));
    if words.words.iter().any(|w| w.base == Base::State) {
        shape(node, LOUD_CHILD);
    }
    let (rec, st) = Rec::declare(px, ik, kf, node, cols, if locked { 4 } else { 2 });
    st_at.set(st);
    let cap = body.iter().filter(|_| locked).find_map(|o| match crate::frame::iface::demand(o) {
        Some(d) => if let Bound::Const(n) = d.count { Some(*n) } else { None },
        None => None,
    });
    // D2780: a calm fill reads on to its window's end at each pull (fires: spool.pull.reads_on)
    let rest = crate::lifetime::material::pull_reads_on(gx, kf);
    if rest {
        fires::fire("spool.pull.reads_on"); // shapecheck fire signal (D2780): a calm pulled Material child read a window at a time
    }
    mcx::pg_box_dyn!(px.mcx(), Reader { child: Some(Child { src, steps, gates, at, words, rest }), arms: None, rec, st, w_j, node, held: None, fresh: None, cap } => dyn SourceMech + 'g)
}

/// The steps of the Material's fill pipeline `kf`'s first `n` ops (its child's, or a pulled union's `Union`, filters and
/// projections), made under the reader's identity (the caller's `Px::making`). D4204: a LOCK-FILL child's LIMIT is a
/// `PulledLimit` over the record's words `st` names.
fn child_steps<'g>(px: &mut Px<'g, '_>, kf: usize, n: usize, st: Option<PgRc<'g, std::cell::Cell<u16>>>) -> PgVec<'g, PgBox<'g, dyn StepMech + 'g>> {
    let (gx, node) = (px.gx, px.gx.pipelines()[kf].node_id);
    let ops = &gx.pipelines()[kf].ops;
    let mcx = px.mcx();
    // D3498: a Result child's gates are the child's open checks (`make_reader`), never steps
    let gate = |j: usize| matches!(gx.pipelines()[kf].source, Source::Const) && matches!(ops[j], Op::Gate(_));
    let pulled = |j: usize| match (crate::frame::iface::demand(&ops[j]), &st) {
        (Some(d), Some(st)) => Some(PulledLimit { offset: if let Bound::Const(m) = d.offset { *m } else { 0 }, st: st.clone(), mcx }),
        _ => None,
    };
    (0..n)
        .filter(|&j| !gate(j))
        .map(|j| match pulled(j) {
            Some(l) => mcx::pg_box_dyn!(mcx, l => dyn StepMech + 'g),
            None => (crate::frame::iface::kind_impl::<StepMaker>(&node, gx.step_need(kf, j)).0)(px, kf, j),
        })
        .collect_in(mcx)
}

/// D4204: the LIMIT / OFFSET of a pulled LOCK-FILL child (rerun/spool/form.rs `lock_fill_pulls`), ExecLimit over its
/// LockRows: an OFFSET row is locked and dropped, every other row counted as it passes on, and the reader ends the child
/// at the count before it reads again (`Reader::cap`), so no row past it is locked. Its counters are the record's
/// per-execution words after the child's end and open (`st` + 2 emitted, + 3 skipped), zeroed at bind: one fill a run.
struct PulledLimit<'g> {
    offset: i64,
    st: PgRc<'g, std::cell::Cell<u16>>,
    mcx: Mcx<'g>,
}

impl<'g> StepMech for PulledLimit<'g> {
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        let st = self.st.get();
        let types: PgVec<'_, Type> = row.carry.params().iter().map(|&v| b.value_type(v)).collect_in(self.mcx);
        let l_cont = b.create_block(&types[..]);
        if self.offset > 0 {
            let sk = b.load(Type::I64, env.params, (st + 3) as i32 * 8);
            let off = b.iconst(Type::I64, self.offset);
            let skip = b.icmp(IntCond::Slt, sk, off);
            let (l_skip, l_keep) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(skip, (l_skip, &[]), (l_keep, &[]));
            b.switch_to(l_skip);
            let sk1 = b.int_bin(IntBinOp::Add, sk, env.one);
            b.store(env.params, (st + 3) as i32 * 8, sk1);
            b.br(l_cont, &row.carry.params());
            b.switch_to(l_keep);
        }
        let em = b.load(Type::I64, env.params, (st + 2) as i32 * 8);
        let em1 = b.int_bin(IntBinOp::Add, em, env.one);
        b.store(env.params, (st + 2) as i32 * 8, em1);
        let c = next(b, env, row);
        b.br(l_cont, &c.params());
        b.switch_to(l_cont);
        Carry::from_params(b.block_params(l_cont), env.mcx)
    }
}

/// G2(e), S1u's pulled union (rerun/spool/form.rs `union_pull`): its arms, made inside the reader (rerun/inner_arms.rs
/// `pulled`: PostgreSQL's Append order, one row per window), the emit's ops (its `Union`, filters and projections) over
/// each arm's row, the per-open words written once at the first pull, and the position word `Env::pos` names.
struct Pulled<'g> {
    src: PgBox<'g, dyn SourceMech + 'g>,
    steps: PgVec<'g, PgBox<'g, dyn StepMech + 'g>>,
    words: OpenWords<'g>,
    pos: i32,
}

/// The reader `ik` of the Material fill `kf` over a settled union, pulled arm after arm as PostgreSQL's Material pulls its
/// Append: each child PostgreSQL pruned at its executor start keeps its skip bit (append/prune.rs, declared here in arm
/// order) and is never opened; a row is made when a reader first reaches it, appended to the record, and read back at
/// every re-open (S1's window rule).
fn union_reader<'g>(px: &mut Px<'g, '_>, (ik, kf): (usize, usize), cols: &NodeList<'_>, w_j: i32, node: i32) -> PgBox<'g, dyn SourceMech + 'g> {
    let gx = px.gx;
    let feeds = crate::rerun::spool::form::union_pull(gx, kf).unwrap_or_else(|why| shape(node, why));
    let fp = &gx.pipelines()[kf];
    let skips = crate::rerun::union_arms::pull_skips(px, kf, &feeds, node);
    // D3612: a run-time pruning over InitPlans that may raise or act, decided at the first pull
    let late = match fp.ops.first() { Some(Op::Append(AppendDesc::Union { prune: Some(d), .. })) if crate::rerun::spool::form::pull_late(gx, kf) => Some(d), _ => None };
    let src = crate::rerun::inner_arms::pulled(px, ik, &feeds, skips, late, node);
    let body = &fp.ops[..fp.ops.len() - 1];
    let ident = px.ident(ik);
    let steps = px.making(kf, ident, |px| child_steps(px, kf, body.len(), None));
    let mut words = OpenWords::new_in(px.mcx());
    src.open_words(&mut words);
    steps.iter().for_each(|s| s.open_words(&mut words));
    if words.words.iter().any(|w| w.base == Base::State) || steps.iter().any(|s| s.expand().is_some()) {
        shape(node, LOUD_CHILD);
    }
    let pos = px.frame_word(WordKey::Open { face: Face::Nest, k: ik, j: u64::MAX, i: 3 }) as i32 * 8;
    let (rec, st) = Rec::declare(px, ik, kf, node, cols, 2);
    mcx::pg_box_dyn!(px.mcx(), Reader { child: None, arms: Some(Pulled { src, steps, words, pos }), rec, st, w_j, node, held: None, fresh: None, cap: None } => dyn SourceMech + 'g)
}

impl Reader<'_> {
    fn word(&self, b: &mut FunctionBuilder<'_>, env: &Env, i: u16) -> Value {
        b.load(Type::I64, env.params, (self.st + i) as i32 * 8)
    }
}

impl SourceMech for Reader<'_> {
    fn declare(&mut self, pb: &mut ProgramBuilder) {
        if let Some(c) = self.child.as_mut() {
            c.src.declare(pb);
            c.steps.iter_mut().for_each(|s| s.declare(pb)); // a step's own hosts, after its source (frame.rs's order)
        }
        if let Some(a) = self.arms.as_mut() {
            a.src.declare(pb);
            a.steps.iter_mut().for_each(|s| s.declare(pb));
        }
    }

    fn open_words(&self, w: &mut OpenWords) {
        w.declare(OpenWord::open("nest.spool position", Base::Words, self.w_j, Type::I64, 0));
    }

    /// The re-open writes the position (its open word); the child opens at its first read. D3614: a fresh word's emit
    /// empties the record when no feed opened in this run, and clears the word.
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        if let Some(w) = self.fresh {
            fresh_open(b, env, &self.rec, w, false);
        }
        env.zero
    }

    /// The records from the position to the count, or one more read from the child when there are none (module doc).
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let (l_more, l_ready) = (b.create_block(&[]), b.create_block(&[]));
        let j = env.load_word(b, self.w_j);
        let n = self.rec.count(b, env);
        let held = b.icmp(IntCond::Slt, j, n);
        b.cond_br(held, (l_ready, &[]), (l_more, &[]));
        b.switch_to(l_more);
        // S1u: the pulled union's next row (its arms read on in Append order), appended
        if let Some(a) = self.arms.as_ref() {
            let (ended, l_fill) = (self.word(b, env, 0), b.create_block(&[]));
            let done = b.icmp(IntCond::Ne, ended, env.zero);
            b.cond_br(done, (l_eof, &[env.zero]), (l_fill, &[]));
            b.switch_to(l_fill);
            crate::rerun::spool::child::fill(b, env, (&*a.src, &a.steps, &a.words), Ok(a.pos), (self.st, self.node), (l_eof, l_ready), &mut |b, env, r| {
                self.rec.append(b, env, r);
                Carry::new(env.one, env.mcx)
            });
            b.switch_to(l_ready);
            let end = self.rec.count(b, env);
            return Win { cursor: j, end, facts: WinConds::default() };
        }
        // the eager form's spool is whole: its count is its end (D1108's held form: or the record its fill raised at)
        let Some(c) = self.child.as_ref() else {
            if let Some(i) = self.held {
                raise_held(b, env, i);
            }
            b.br(l_eof, &[env.zero]);
            b.switch_to(l_ready);
            let end = self.rec.count(b, env);
            return Win { cursor: j, end, facts: WinConds::default() };
        };
        let (ended, l_fill) = (self.word(b, env, 0), b.create_block(&[]));
        let done = b.icmp(IntCond::Ne, ended, env.zero);
        b.cond_br(done, (l_eof, &[env.zero]), (l_fill, &[]));
        b.switch_to(l_fill);
        // D4204: a LOCK-FILL child's LIMIT met: ExecLimit returns at its window's end without pulling the LockRows again
        if let Some(n) = self.cap {
            let (em, l_go) = (self.word(b, env, 2), b.create_block(&[]));
            let n = b.iconst(Type::I64, n);
            let met = b.icmp(IntCond::Sge, em, n);
            b.cond_br(met, (l_eof, &[env.zero]), (l_go, &[]));
            b.switch_to(l_go);
        }
        c.fill(b, env, self.st, self.node, l_eof, l_ready, &mut |b, env, r| {
            self.rec.append(b, env, r);
            Carry::new(env.one, env.mcx)
        });
        b.switch_to(l_ready);
        let end = self.rec.count(b, env);
        Win { cursor: j, end, facts: WinConds::default() }
    }

    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        self.rec.read(b, env, j, carry, next)
    }

    /// The position: the record last handed on (nest/inner.rs `set_cursor`), or the window's end.
    fn write_back(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, cur: Value, _out: Value) {
        env.store_word(b, self.w_j, cur);
    }
}

/// D1108's held form at the record count: 1 from `m11_run_held` (the fill's error held, its region stamped `RUN_HELD`,
/// the error stashed again for this thread) leaves by the raise exit, as merge.join's held inner does (held_inner.rs).
fn raise_held(b: &mut FunctionBuilder<'_>, env: &mut Env, i: u64) {
    use gravity_abi::status::raise;
    use gravity_ir::HostFlags;
    use gravity_kernels::host_abi::m11_run_held;
    let flags = HostFlags { may_error: true, ..HostFlags::default() };
    let h = b.declare_host_once("m11_run_held", &[Type::Ptr, Type::I64], Some(Type::I64), gravity_abi::host_words!(m11_run_held(ctx, k)), flags);
    let i = b.iconst(Type::I64, i as i64);
    let st = b.call_host(h, &[env.ctx, i], Some(Type::I64)).expect("m11_run_held returns its answer");
    let (held, l_raise, l_on) = (b.icmp(IntCond::Ne, st, env.zero), b.create_block(&[]), b.create_block(&[]));
    b.cond_br(held, (l_raise, &[]), (l_on, &[]));
    b.switch_to(l_raise);
    b.mark_cold(l_raise);
    let e = b.iconst(Type::I64, raise::bit(raise::HOST) as i64);
    b.br(env.l_raise, &[e]);
    b.switch_to(l_on);
}

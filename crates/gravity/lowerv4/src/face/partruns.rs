//! D3336, road `win.partruns` (default off): a window read by claims. The window's fill stays the one serial pipeline it
//! is (the plan's order: every partition's rows arrive together), but it stores each partition whole into one of several
//! window objects, the next object taking the next partition (kernels `m10_win_put_part`; exec ops/window/proto.rs lays
//! the objects, `WINDOW_PARTED`). The reader is eligible and its claims are the objects, one each (the Window protocol's
//! execution answers them): each runs PG's nodeWindowAgg over its own partitions, which no other object holds, so every
//! row's function values are the serial window's. The rows leave in another order, so the reader's end must be one no row
//! order reaches (a hash build: `effects::visible::wide_build_visible`; a fold: `wide_visible`).
//!
//! The form (`decide`): the road on; a PARTITION BY; the window the reader's first op (no SubPlan step ahead of it), no
//! projection evaluated first, no VOLATILE or lazy cell, no pulled fill, no frame offset, every function argument and
//! FILTER a column or a constant (no cell the fill evaluates can raise: no held error waits in an object); neither
//! pipeline in a rerun set, a recheck, a lazy build or the leader's own list; the reader's end a breaker no row order
//! reaches; a claims site upstream of the reader (its nominal driving site). Exec checks the rest at prepare (each function
//! the objects call one a pool thread may run, the reader lowered eligible) and else lays one object: the serial form's
//! rows. The window's faces (ops/window/rows.rs, lower.rs) read it here. A helper file (ROWS: the road's row). Owner
//! M10-WINDOW.
use gravity_decompose::ops::window::WindowDesc;
use gravity_decompose::Source;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Type, Value};
use gravity_program::protos::{Proto, ProtoKind, WINDOW_PARTED, WINDOW_PARTS_AT, WINDOW_PARTS_COUNT};
use types_nodes::Node;

use crate::face::runs::WIN_PARTRUNS;
use crate::frame::env::Env;
use crate::frame::px::Px;

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(WIN_PARTRUNS, &())], ..crate::ops::NONE };

/// A window read by claims: its pipelines, its table word and the reader's nominal site.
#[derive(Clone, Copy)]
pub(crate) struct Parted {
    pub(crate) fill: usize,
    pub(crate) reader: usize,
    /// The Window proto's handle word (the objects' table).
    pub(crate) table: u16,
    /// The reader's nominal driving site (a claims site upstream: the runner reads no row through it).
    pub(crate) site: usize,
    /// D3336 (A1a, face/partfill.rs `win.partfill`): the fill runs as claims too, over its sorted wide emit's K-cut parts:
    /// that emit's run table word (`WINDOW_CUT`); `None`: the serial fill deals the partitions.
    pub(crate) cut: Option<u16>,
}

/// The road on for this statement (default off: road_off.rs `DEFAULT_OFF`).
pub(crate) fn road_on(px: &Px<'_, '_>) -> bool {
    crate::ops::sort::strategies::resident::on(px, WIN_PARTRUNS)
}

/// A column or a constant (through a relabel): a cell the fill stores raises nothing.
fn plain(e: Node<'_>) -> bool {
    let e = e.as_relabel_type().map_or(e, |r| r.arg);
    e.as_var().is_some() || e.as_const().is_some()
}

/// Whether pipeline `k` reads a window by claims: its source runs wide though the decomposer's descriptor is serial, so
/// a row sort or top at its end takes section ends, as over a wide group emit (`Px::wide`; ops/sort/lower.rs,
/// topk_rows.rs). Its window's fill lowers first (it produces the buffer `k` reads), so the form is known by then.
pub(crate) fn reads_wide(px: &Px<'_, '_>, k: usize) -> bool {
    road_on(px) && crate::ops::window::lower::parted_reader(px, k)
}

/// Whether pipeline `q` reads breaker slot `slot` (its source).
fn reads(gx: &crate::graph::Gx<'_, '_>, q: usize, slot: u8) -> bool {
    matches!(gx.pipelines()[q].source, Source::Breaker { slot: s } if s == slot)
}

/// Whether pipeline `p` fills the run that pipeline `l` (a producer of it, `life_of`'s) ends: both end in its slot and no
/// pipeline between them reads it (a FULL join's probe and its build drain fill one Sort; a reader between would end one
/// life of the slot and start another).
fn same_run(gx: &crate::graph::Gx<'_, '_>, p: usize, l: usize) -> bool {
    let slot = |q: usize| crate::frame::iface::end_breaker_of(gx, q).map(|b| b.slot);
    match (slot(p), slot(l)) {
        (Some(a), Some(b)) if a == b && p <= l => (p + 1..=l).all(|q| !reads(gx, q, a)),
        _ => false,
    }
}

/// A claims site upstream of pipeline `k`: its own, else one of the producers of the breaker it reads (every producer of
/// that run: `same_run`), and so on; a depth-first walk over at most `UP` pipelines held at once and twice the program's
/// pipelines visited.
fn upstream_site(px: &Px<'_, '_>, k: usize) -> Option<usize> {
    const UP: usize = 16;
    let gx = px.gx;
    let np = gx.pipelines().len();
    let (mut stack, mut n, mut seen) = ([0usize; UP], 1usize, 0usize);
    stack[0] = k;
    // bounded("other: at most twice the program's pipelines visited, at most UP held")
    while n > 0 && seen < 2 * np {
        n -= 1;
        let q = stack[n];
        seen += 1;
        if let Some(s) = px.claims_site(q) {
            return Some(s);
        }
        let Source::Breaker { slot } = gx.pipelines()[q].source else { continue };
        let Some(l) = gx.life_of(q, slot).map(|l| l.0 as usize).filter(|&l| l < q) else { continue };
        // bounded("other: one step per pipeline up to the run's last producer, at most the program's pipelines")
        for p in (0..=l).rev() {
            if n < UP && same_run(gx, p, l) {
                stack[n] = p;
                n += 1;
            }
        }
    }
    None
}

/// The form for WindowAgg `d` filled by pipeline `fill` and read by `reader`: its nominal site when it holds (`decide` in
/// the module's text). Read before any proto of the window is declared.
pub(crate) fn decide<'p>(px: &Px<'_, 'p>, d: &WindowDesc<'p>, node: i32, fill: usize, reader: usize) -> Option<usize> {
    use crate::ops::window::lower::{lazy_cells, proj_first, pulled, window_after_args};
    use types_nodes::rawnodes::{FRAMEOPTION_END_OFFSET, FRAMEOPTION_START_OFFSET};
    let gx = px.gx;
    if !road_on(px) || fill >= gx.pipelines().len() || reader >= gx.pipelines().len() || d.partition.is_empty() {
        return None;
    }
    let r = &gx.pipelines()[reader];
    let quiet = !d.volatile_args && !d.volatile_proj && !pulled(px, node) && !lazy_cells(gx, d).contains(&true) && d.plan.frameOptions & (FRAMEOPTION_START_OFFSET | FRAMEOPTION_END_OFFSET) == 0;
    let first = window_after_args(&r.ops).is_some_and(|(w, _)| w == 0) && proj_first(px, reader) == 0;
    let cells = d.functions.iter().all(|f| f.func.args.iter().chain(f.func.aggfilter).all(plain));
    let rerun = |q: usize| gx.graph.reruns.iter().any(|s| s.pipelines.contains(&(q as u16)));
    let recheck = gx.graph.recheck || gx.pipelines().iter().any(|q| q.ops.iter().any(|op| matches!(op, gravity_decompose::Op::Write(_) | gravity_decompose::Op::Lock(_))));
    // a hash build takes every row before any reader reads one, in an order its readers never see (D4532's reading);
    // any other end: `wide_visible`'s, a sort whose run only windows read taken as free (`sort_into_windows`)
    use gravity_abi::breaker::BreakerKind as B;
    // the ends that take a claims-driven source as their own: a hash build, a fold, a row sort or top (`reads_wide`); not a
    // run a merge join reads (its end is face/slot_run.rs's), a Materialize or a Distinct
    let end = crate::frame::iface::end_breaker(gx, reader);
    let ends = match end {
        Some(B::HashBuild | B::HashAgg | B::PlainAgg) => true,
        Some(B::Sort | B::TopK) => matches!(gx.end_need(reader), crate::face::kinds::KindId::RunsSort | crate::face::kinds::KindId::RunsTopkRows),
        _ => false,
    };
    let build = end == Some(B::HashBuild);
    let visible = if build { crate::effects::visible::wide_build_visible(gx, reader) } else { crate::effects::visible::wide_visible(gx, reader) };
    let order_free = r.desc.produces().is_some() && (visible.is_none() || matches!(visible, Some(crate::effects::visible::Why::Rows)) && sort_into_windows(px, reader));
    let alone = [fill, reader].into_iter().all(|q| !rerun(q) && !px.serial.contains(&q) && !px.lazy.iter().any(|l| l.0 == q || l.1 == q));
    if !quiet || !first || !cells || !ends || !order_free || recheck || !alone {
        return None;
    }
    upstream_site(px, reader)
}

/// Pipeline `reader` ends in a Sort (one producer of its run, or several: `same_run`) whose every run reader fills a
/// WindowAgg's buffer and folds no group of its own:
/// `wide_visible` reads such a run as visible (`run_visible`: a reader that hands the run on to another breaker), but the
/// order the sort's ties come in reaches only those windows, and SQL leaves a window's peer order unspecified (any order
/// among peers is correct, as the result checks read window outputs: tie-aware). The sort's keys are its window's
/// PARTITION BY and ORDER BY keys, or finer, so its ties are that window's peers; the sort is unstable at every width
/// (ops/sort raw.rs).
fn sort_into_windows(px: &Px<'_, '_>, reader: usize) -> bool {
    let gx = px.gx;
    let Some(run) = crate::frame::iface::end_breaker_of(gx, reader) else { return false };
    if run.kind != gravity_abi::breaker::BreakerKind::Sort {
        return false;
    }
    let ps = gx.pipelines();
    // the run's readers: those whose producer (`life_of`: its last) fills the run `reader` fills (`same_run`)
    let mut readers = (reader + 1..ps.len()).filter(|&q| reads(gx, q, run.slot) && gx.life_of(q, run.slot).is_some_and(|l| same_run(gx, reader, l.0 as usize))).peekable();
    if readers.peek().is_none() {
        return false;
    }
    // bounded("other: one step per pipeline past the reader, at most the program's pipelines")
    readers.all(|q| crate::ops::window::rows::reader_opt(px, q).is_some() && !ps[q].ops.iter().any(|op| matches!(op, gravity_decompose::Op::GroupRun(_))))
}

/// The window's proto (`handle` calls this for a window `decide` admits): the Window proto over its objects' table (`arg`
/// with `WINDOW_PARTED`, its pipeline the fill, its peer the reader, whose claims its execution answers). The form's facts.
/// D3336 (A1a): `cut`, the fill's sorted wide emit's run table word when its fill runs as claims (`WINDOW_CUT`).
pub(crate) fn declare(px: &mut Px<'_, '_>, arg: u64, fill: usize, reader: usize, site: usize, cut: Option<u16>) -> Parted {
    let table = px.proto_words(1);
    let arg = arg | WINDOW_PARTED | if cut.is_some() { gravity_program::protos::WINDOW_CUT } else { 0 };
    px.declare_proto(Proto::new(ProtoKind::Window, fill as u16).peer(reader as u16).words(table, 1).arg(arg));
    fires::fire("lower.win.partruns"); // shapecheck fire signal (D3336): a window lowered to be read by claims
    if cut.is_some() {
        fires::fire("lower.win.partfill"); // shapecheck fire signal (D3336 A1a): a parted window's fill lowered to run as claims
    }
    Parted { fill, reader, table, site, cut }
}

/// The table's address (exec's lay, alive for the execution).
fn table(b: &mut FunctionBuilder<'_>, env: &Env, p: &Parted) -> Value {
    b.load(Type::Ptr, env.params, p.table as i32 * 8)
}

/// The object a reader's claim reads (`m10_win_part`; 0: no object, a claim of none); a claim the table does not cover
/// raises by name.
pub(crate) fn object(b: &mut FunctionBuilder<'_>, env: &Env, p: &Parted) -> Value {
    let tbl = table(b, env, p);
    let f = crate::ops::window::lower::host(b, "m10_win_part", &[Type::Ptr, Type::Ptr], gravity_abi::host_words!(gravity_kernels::ops::window::entries::m10_win_part(ctx, tbl)));
    let h = b.call_host(f, &[env.ctx, tbl], Some(Type::I64)).expect("m10_win_part returns the object");
    crate::ops::window::lower::raise_below_zero(b, env, None, h);
    h
}

/// The object whose turn it is (table word `n + 1`'s), which a fill's cell error would mark (none can raise: `decide`).
pub(crate) fn current(b: &mut FunctionBuilder<'_>, env: &Env, p: &Parted) -> Value {
    let tbl = table(b, env, p);
    let n = b.load(Type::I64, tbl, 8 * WINDOW_PARTS_COUNT as i32);
    let past = b.ptr_add(tbl, n, 8, 8 * WINDOW_PARTS_AT as i32);
    let turn = b.load(Type::I64, past, 0);
    let at = b.ptr_add(tbl, turn, 8, 8 * WINDOW_PARTS_AT as i32);
    b.load(Type::I64, at, 0)
}

/// `m10_win_put_part(ctx, tbl, rec)`: record `rec` stored in the object whose turn it is, the turn passing on at a new
/// partition; its status.
pub(crate) fn put(b: &mut FunctionBuilder<'_>, env: &Env, p: &Parted, rec: Value) -> Value {
    let tbl = table(b, env, p);
    let f = crate::ops::window::lower::host(b, "m10_win_put_part", &[Type::Ptr, Type::Ptr, Type::Ptr], gravity_abi::host_words!(gravity_kernels::ops::window::entries::m10_win_put_part(ctx, tbl, rec)));
    b.call_host(f, &[env.ctx, tbl, rec], Some(Type::I64)).expect("m10_win_put_part returns its status")
}

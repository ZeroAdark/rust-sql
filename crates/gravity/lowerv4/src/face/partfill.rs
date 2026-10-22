//! D3336 (A1a), road `win.partfill` (default off): a window read by claims (face/partruns.rs, `win.partruns`) whose fill
//! runs as claims too. The fill stays one pipeline: the drain of a sorted wide emit (table/emit_runs.rs, its claims' runs
//! merged by key range at the section end under `emit.parmerge`) into the window's buffer. Its sort is the Sort under the
//! WindowAgg, so its leading keys are the window's PARTITION BY (K); the section end cuts its parts on K alone (kernels
//! `agg_merge_runs_part_k`: every run cut at the lower bound of a splitter's K), so each partition lies in one part and
//! the parts laid one after another are still the one merge. The fill's claims are the window's objects, one each (the
//! Window protocol's execution answers them, `WINDOW_CUT`): claim `i` reads its share of the parts (`agg_runs_claim`) and
//! stores them in object `i`, which then holds whole partitions in the plan's order, as the dealing fill's objects do;
//! the reader's claims read one object each, as before. When the section end made no cut (too few records or runs, a
//! refused array) claim 0 reads every record and deals them as the serial fill does, the other claims none.
//!
//! The form: both roads on (this and `win.partruns`), the window parted (`partruns::decide`), its fill the plain drain of a
//! top over a sorted wide emit that merges in parallel (`emit_runs::parallel`), handing the window the top's rows through
//! filters and projections, the sort's first K keys the PARTITION BY columns (as a set, walked back through the
//! projections), each K key of a collation whose equal bytes are equal values (`kcut`); the drain takes it at its make
//! (topn/lower.rs `make_drain`: `cut_form`), before the window's end asks its `handle`.
//!
//! The row sort's form (`rows_form`): a HashAggregate's groups sorted for a window never take form G (ops/sort/shapes.rs
//! `group_sort`: the window's fill produces into the slot the drained group frees, D295), so the fill is the reader of a
//! full row sort (runs.sort) instead. There the cut is exec's, after the sort ran (exec ops/sort/cosplit.rs, a CoSplit proto
//! with `COSPLIT_WINDOW`): the sort's one sealed run (runs.sort's phase 2 merged the workers' runs into it) cut on its
//! first K keys by the sort's own order (its row shape's compare: images, collations, bpchar trim), into as many parts as
//! the window has objects, so equal partition keys never straddle a cut; the fill's claim of object `i` reads part `i` as a
//! co-split outer reads its range (ops/sort/lower.rs `RunEmit`) and stores into object `i`. Uncut (too few rows, put runs,
//! an order the cut cannot take: a user comparator), claim 0 reads every row and deals the partitions. A helper file
//! (ROWS: the road's row). Owner M10-WINDOW.
use gravity_decompose::ops::shared::sort::SortForm;
use gravity_decompose::{Op, Source};
use gravity_program::protos::{Proto, ProtoKind};

use crate::face::kinds::Face;
use crate::frame::px::{Px, WordKey};
use crate::table::emit_group::Emit;

crate::strategies! {
    /// D3336 (A1a): a parted window's fill read by claims over its sorted wide emit's parts, cut on its PARTITION BY.
    WIN_PARTFILL = 163, "win.partfill", Window; // a3-strategy-bits.tsv row 163 (after A2 161, F2b 162)
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(WIN_PARTFILL, &())], ..crate::ops::NONE };

/// The road on for this statement (default off: road_off.rs `DEFAULT_OFF`).
pub(crate) fn road_on(px: &Px<'_, '_>) -> bool {
    crate::ops::sort::strategies::resident::on(px, WIN_PARTFILL)
}

/// The K the section end of top pipeline `t` (its sort, `words` sort words: one a key) cuts its parts on: the count of the
/// sort's leading keys a window reading the top's drain is partitioned by, 0 for none. The drain is the pipeline that
/// reads `t`'s slot with `t` its producer, and hands the window's fill the top's rows through filters and projections
/// only; its window the one whose reader reads the drain's buffer; the window's PARTITION BY columns, walked back through
/// the drain's projections to the sort row's columns (`sort_column`: plain columns only), are the sort's first K key
/// columns as a set, and every K key's collation holds equal values as equal bytes (`KeyColl::bytes_eq`), so equal under
/// the sort's comparator is one partition. Structural: the top's lowering and the drain's read it alike.
pub(crate) fn kcut(px: &Px<'_, '_>, t: usize, words: usize) -> u8 {
    if !road_on(px) || !crate::face::partruns::road_on(px) {
        return 0;
    }
    let gx = px.gx;
    let ps = gx.pipelines();
    let Some(sd) = ps.get(t).and_then(|p| p.ops.iter().find_map(|op| if let Op::Sort(d) = op { Some(d) } else { None })) else { return 0 };
    let Some(slot) = crate::frame::iface::end_breaker_of(gx, t).map(|b| b.slot) else { return 0 };
    // bounded("other: one step per pipeline past the top, at most the program's pipelines")
    let Some(drain) = (t + 1..ps.len()).find(|&q| matches!(ps[q].source, Source::Breaker { slot: s } if s == slot) && gx.life_of(q, slot).is_some_and(|l| l.0 as usize == t)) else { return 0 };
    if !ps[drain].ops.iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_))) {
        return 0;
    }
    let Some(reader) = crate::ops::window::rows::reader_opt(px, drain) else { return 0 };
    let Some((_, w)) = crate::ops::window::lower::window_after_args(&ps[reader].ops) else { return 0 };
    let n = w.partition.len();
    if n == 0 || n > sd.keys.len() || n > words || n >= u8::MAX as usize {
        return 0;
    }
    let lead = &sd.keys[..n];
    // the window's PARTITION BY columns are the drain's (past its projections): the sort row's columns they read
    let col = |resno: i16| sort_column(&ps[drain].ops, resno);
    let covers = w.partition.iter().all(|p| col(p.resno).is_some_and(|c| lead.iter().any(|k| k.resno == c))) && lead.iter().all(|k| w.partition.iter().any(|p| col(p.resno) == Some(k.resno)));
    if covers && lead.iter().all(|k| k.coll.bytes_eq) { n as u8 } else { 0 }
}

/// The frame word of the object a K-cut fill's claim stores into (`i` 3: beside a reader's, rows.rs `object_word`), filed
/// by the drain's open (`agg_runs_claim`) and read by the window's end (rows.rs `WinEnd`); 0: the window deals.
pub(crate) fn object_word(px: &mut Px<'_, '_>, k: usize) -> i32 {
    px.frame_word(WordKey::Open { face: Face::Window, k, j: u64::MAX, i: 3 }) as i32 * 8
}

/// The drain `emit` of pipeline `k` as a K-cut fill (topn/lower.rs `make_drain`): a plain merging drain (`Emit::plain_drain`)
/// whose top's section end cuts on K (`SortedRuns::kcut`) and whose rows fill a window's buffer. The drain offers the fill
/// (window/lower.rs `offer_cut`) and asks the window's handle; when the window is parted its fill runs as claims
/// (`WINDOW_CUT`): the window's object table word, the claim's object word, and the window's claims site (the drain's
/// nominal driving site, as the reader's). `None`: the drain as before.
pub(crate) fn cut_form(px: &mut Px<'_, '_>, k: usize, emit: &Emit<'_, '_>) -> Option<(u16, i32, usize)> {
    let r = emit.runs()?;
    if !road_on(px) || !emit.plain_drain() || r.kcut == 0 {
        return None;
    }
    let gx = px.gx;
    let reader = crate::ops::window::rows::reader_opt(px, k)?;
    let (_, w) = crate::ops::window::lower::window_after_args(&gx.pipelines()[reader].ops)?;
    let node = w.plan.plan.plan_node_id;
    crate::ops::window::lower::offer_cut(px, k, r.word);
    crate::ops::window::lower::handle(px, node, k, w);
    let p = crate::ops::window::lower::parted(px, node).filter(|p| p.fill == k && p.cut.is_some())?;
    Some((p.table, object_word(px, k), p.site))
}

/// D3336 (A1a, the row sort's form): pipeline `k`, the reader of full row sort `producer`'s run of `shape`, as a parted
/// window's fill whose claims read K-cut parts of the run: both roads on (this and `win.partruns`); `k` the sort's one
/// reader (no grouping set's other arm reads the run), its ops filters and projections, ending in the buffer of the window its reader
/// pipeline reads; the sort a full one (`SortForm::Runs`, no presorted prefix, no union arm); its first K keys the
/// window's PARTITION BY columns as a set (each the sort row's column a plain column of the fill's projections hands on,
/// `sort_column`; K at most `COSPLIT_MAX_KEYS`), none under a user comparator; no rerun set,
/// lazy pair or serial pin on either. The window's handle is asked here (the fill offered, `offer_cut`); when it parts the
/// window, the CoSplit proto of the cut is declared after it (exec reads the window's object count at its prepare) and the
/// fill reads as its outer: the range table's word, the window's claims site and the run's rows, with the window's object
/// table word (the claim's object, `object_word`). `None`: the fill as before.
pub(crate) fn rows_form(px: &mut Px<'_, '_>, k: usize, producer: usize, shape: &crate::face::runs::Shape) -> Option<(crate::face::cosplit::At, u16)> {
    if !road_on(px) || !crate::face::partruns::road_on(px) {
        return None;
    }
    let gx = px.gx;
    let ps = gx.pipelines();
    if k >= ps.len() || producer >= ps.len() || px.ident(k) != k || crate::frame::iface::breaker_of(&ps[k].desc).is_none() {
        return None;
    }
    let Source::Breaker { slot } = ps[k].source else { return None };
    let life = gx.life_of(k, slot);
    // bounded("other: one step per pipeline, at most the program's pipelines")
    let readers = (0..ps.len()).filter(|&j| ps[j].desc.consumes.contains(&slot) && gx.life_of(j, slot) == life).count();
    if readers > 1 || !ps[k].ops.iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_))) {
        return None;
    }
    let reader = crate::ops::window::rows::reader_opt(px, k)?;
    let (_, w) = crate::ops::window::lower::window_after_args(&ps[reader].ops)?;
    let (sd, _) = crate::ops::sort::lower::sort_of(px, producer);
    if !matches!(sd.form, SortForm::Runs) || sd.presorted > 0 || crate::ops::sort::lower::union_arm(&ps[producer]).is_some() {
        return None;
    }
    let n = w.partition.len();
    if n == 0 || n > sd.keys.len() || n > gravity_abi::program::COSPLIT_MAX_KEYS {
        return None;
    }
    let lead = &sd.keys[..n];
    // the window's PARTITION BY columns are the fill's (past its projections): the sort row's columns they read
    let mut cols = [0i16; gravity_abi::program::COSPLIT_MAX_KEYS];
    for (c, p) in cols.iter_mut().zip(w.partition.iter()) {
        *c = sort_column(&ps[k].ops, p.resno)?;
    }
    let pcols = &cols[..n];
    let covers = pcols.iter().all(|&c| lead.iter().any(|x| x.resno == c)) && lead.iter().all(|x| pcols.contains(&x.resno));
    if !covers || !(0..n).all(|i| shape.keys.get(i).is_some_and(|x| !x.user)) {
        return None;
    }
    let rerun = |q: usize| gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(q as u16)));
    if rerun(k) || rerun(producer) || px.lazy.iter().any(|l| l.0 == k || l.1 == k) || px.serial.contains(&k) {
        return None;
    }
    let node = w.plan.plan.plan_node_id;
    crate::ops::window::lower::offer_cut(px, k, u16::MAX);
    crate::ops::window::lower::handle(px, node, k, w);
    let part = crate::ops::window::lower::parted(px, node).filter(|p| p.fill == k && p.cut.is_some())?;
    let h = px.proto_words(1);
    px.declare_proto(Proto::new(ProtoKind::CoSplit, k as u16).slot(slot).words(h, 1).arg(gravity_program::protos::cosplit_window_arg(part.table, n as u8)));
    fires::fire("lower.win.partfill.rows"); // shapecheck fire signal (D3336 A1a): a parted window's fill over a row sort's K-cut parts
    Some((crate::face::cosplit::At { h, site: part.site, rows: ps[producer].desc.predicted_rows }, part.table))
}

/// The column of the sort's row that column `c` of a fill whose ops are `ops` reads: through its projections (each a
/// plain column of the row below, decompose window.rs `under_sort`'s walk) and filters; none past any other op or a
/// computed column.
fn sort_column(ops: &[Op<'_>], c: i16) -> Option<i16> {
    let mut resno = c;
    for op in ops.iter().rev() {
        match op {
            Op::Project(l) => {
                let t = l.nth((resno as usize).checked_sub(1).filter(|&n| n < l.len())?).as_target_entry().filter(|t| t.resno == resno)?;
                resno = t.expr.as_var().filter(|v| v.varno == types_nodes::primnodes::OUTER_VAR && v.varlevelsup == 0)?.varattno;
            }
            Op::Filter(_) => {}
            _ => return None,
        }
    }
    Some(resno)
}

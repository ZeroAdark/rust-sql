//! The hash join's answers for road key.dependent (ops/agg/strategies/dependent.rs, D3324 (b)), read through frame::iface:
//! whether a group key is the build's one join key (`unique_key`), whether another key is a column of the same build row
//! that the build keeps as its scan lane's code (`code_col`), and whether the build is unique on its key at run time
//! (`unique_now`); D3324 R3 (road key.carry): a payload column of any INNER probe's build (`build_row`). Owner rendertext.
//! Budget 140 lines.
use gravity_decompose::join::JoinDesc;
use gravity_decompose::Op;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type, Value};
use gravity_program::{DictRef, LaneId, Rep};
use types_nodes::jointype::JoinType;
use types_nodes::primnodes::OUTER_VAR;
use types_nodes::Node;

use crate::frame::env::Env;
use crate::frame::px::Px;
use crate::graph::Gx;
use crate::ops::hashjoin::strategies::payload_fp::{end_pos, lane, records_of, walk, From};

/// A group key that is a build's one join key: build pipeline `b`, probed at op `j` of the producer, its region's index
/// among the producer's consumed slots (`None` in the spill pair pass of that probe, which never skips: `q` skips no
/// row then either, the build's header `SPILL`; a pass of a later probe reads the build's header at the same index,
/// spill/pair.rs), and the planner's `inner_unique`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct UniqueKey {
    pub b: usize,
    pub j: usize,
    pub consumed: Option<usize>,
    pub unique: bool,
}

/// Column `col` of the row entering pipeline `q`'s end as the one join key of an INNER probe's build, where the build's
/// records are its scan's rows as built (no swap join.buildside may make, no Arms run).
pub(crate) fn unique_key(px: &mut Px<'_, '_>, q: usize, col: i16) -> Option<UniqueKey> {
    let gx = px.gx;
    let (j, attno) = match walk(gx, q, end_pos(gx, q), col)? {
        From::Inner { j, attno } => (j, Some(attno)),
        From::Scan(a) => (outer_key(gx, q, a)?, None),
    };
    let Op::Join(JoinDesc::Probe { slot, jointype: JoinType::JOIN_INNER, decls, unique, .. }) = &gx.pipelines()[q].ops[j] else { return None };
    let (b, _) = records_of(gx, q, j).filter(|_| decls.len() == 1)?;
    if attno.is_some_and(|a| !same_col(gx, b, decls[0].inner.node(), a)) {
        return None;
    }
    let sh = crate::ops::hashjoin::build_side::shape(px, *slot, b);
    if sh.swapped || crate::ops::hashjoin::build::arms_run(px, b, *slot) {
        return None;
    }
    // the pass pairs the pipeline's last probe (spill/build.rs `alone`: only filters and projections follow it)
    let last = gx.pipelines()[q].ops.iter().rposition(|op| matches!(op, Op::Join(JoinDesc::Probe { .. })));
    let pairing = px.ext::<crate::ops::hashjoin::build_side::JoinExt>(crate::face::kinds::Face::Join).is_some_and(|e| e.pairing.is_some());
    Some(UniqueKey { b, j, consumed: if pairing && last == Some(j) { None } else { px.consumed(q, *slot) }, unique: *unique })
}

/// D3324 R3 (road key.carry): column `col` of the row entering pipeline `q`'s end as a payload column of an INNER probe's
/// build, where the build's records are its scan's rows as built: that build as a `UniqueKey` that never skips (`consumed`
/// `None`, `unique` false), whose code columns `code_col` then names.
pub(crate) fn build_row(px: &mut Px<'_, '_>, q: usize, col: i16) -> Option<UniqueKey> {
    let gx = px.gx;
    let Some(From::Inner { j, .. }) = walk(gx, q, end_pos(gx, q), col) else { return None };
    let Op::Join(JoinDesc::Probe { slot, jointype: JoinType::JOIN_INNER, .. }) = &gx.pipelines()[q].ops[j] else { return None };
    let (b, _) = records_of(gx, q, j)?;
    let sh = crate::ops::hashjoin::build_side::shape(px, *slot, b);
    if sh.swapped || crate::ops::hashjoin::build::arms_run(px, b, *slot) {
        return None;
    }
    Some(UniqueKey { b, j, consumed: None, unique: false })
}

/// Road key.dependent's probe-side key (`GROUP BY e.tenant_id, t.tenant_name` over `JOIN tenants t ON t.tenant_id =
/// e.tenant_id`): the INNER probe of pipeline `q` whose one outer key is scan column `a` of the probe side, the column the
/// group keys by. Every row past that probe holds `a` equal to the record's build key (an INNER probe hands on only
/// matched rows, never a NULL key), so `a` keys the group as the build key does: the build's unique key, the probe `j`.
/// The key names the row entering the probe's child's last projection when decompose read it through that projection
/// (keys.rs `strip`: a later probe's `OUTER_VAR`), else the row entering the probe.
fn outer_key(gx: &Gx<'_, '_>, q: usize, a: i16) -> Option<usize> {
    let ops = &gx.pipelines()[q].ops;
    ops.iter().enumerate().find_map(|(j, op)| {
        let Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_INNER, decls, .. }) = op else { return None };
        let [d] = &decls[..] else { return None };
        let v = crate::table::emit_group::bare(d.outer.node()).as_var().filter(|v| v.varlevelsup == 0 && v.varattno > 0)?;
        let at = match v.varno {
            OUTER_VAR if d.outer.agg() => walk(gx, q, j, v.varattno),
            OUTER_VAR => walk(gx, q, ops[..j].iter().rposition(|op| matches!(op, Op::Project(_)))?, v.varattno),
            r if gx.pipelines()[q].source.relation().is_some_and(|s| r == s.0 as i32) => Some(From::Scan(v.varattno)),
            _ => None,
        };
        (at == Some(From::Scan(a))).then_some(j)
    })
}

/// Whether build key `key` (over the row entering build pipeline `b`'s Build op) is that row's column `a`: both walk back
/// to one scan attribute.
fn same_col(gx: &Gx<'_, '_>, b: usize, key: Node<'_>, a: i16) -> bool {
    let end = end_pos(gx, b);
    let Some(v) = crate::table::emit_group::bare(key).as_var().filter(|v| v.varlevelsup == 0 && v.varattno > 0) else { return false };
    let at = match v.varno {
        OUTER_VAR => walk(gx, b, end, v.varattno),
        r if gx.pipelines()[b].source.relation().is_some_and(|s| r == s.0 as i32) => Some(From::Scan(v.varattno)),
        _ => None,
    };
    matches!(at, Some(From::Scan(_))) && at == walk(gx, b, end, a)
}

/// Column `col` of the row entering pipeline `q`'s end as a payload column of `u`'s build that the build keeps as its
/// scan lane's code (build.rs: a code lane's cell, not a column hoist.rs replaced by a call's verdict): its INNER_VAR attno
/// and the code rep, its table dropped (rep.code.hash renders it, or reads join.payload_fp's words). `bp`: the key is
/// bpchar (then the column is a bpchar of declared length, payload_fp.rs `lane`).
pub(crate) fn code_col(px: &mut Px<'_, '_>, q: usize, u: &UniqueKey, col: i16, bp: bool) -> Option<(i16, Rep)> {
    let gx = px.gx;
    let Some(From::Inner { j, attno }) = walk(gx, q, end_pos(gx, q), col) else { return None };
    let Op::Join(JoinDesc::Probe { slot, .. }) = &gx.pipelines()[q].ops[j] else { return None };
    let sh = crate::ops::hashjoin::build_side::shape(px, *slot, u.b);
    if j != u.j || !sh.payload.contains(&attno) || sh.hoist.get().is_some_and(|h| h.col == attno) {
        return None;
    }
    let (Some(From::Scan(a)), Some(_)) = (walk(gx, u.b, end_pos(gx, u.b), attno), lane(gx, u.b, attno, bp)) else { return None };
    // the lane as the build's site numbered it (made before this pipeline), its records' cells' rep: emit_group.rs `site_rep`
    let rep = |s: &gravity_program::SiteDesc| s.lanes.iter().find(|l| l.id == LaneId::Attr(a)).map(|l| l.rep);
    let numbered = px.pipeline_site(u.b).and_then(|i| px.sites.get(i)).and_then(rep);
    match numbered.or_else(|| px.site_mut(u.b).and_then(|s| rep(s)))? {
        Rep::Code { dict } => Some((attno, Rep::Code { dict: DictRef { fp_table: DictRef::NO_TABLE, ..dict } })),
        _ => None,
    }
}

/// Whether `u`'s build is unique on its key now (`Bool`), read from its header in the producer's row: planner-proved, or
/// laid out DENSE (build.rs) with no duplicate and no spilled partition (spill/build.rs `SPILL`); `None` where the form
/// never skips (a spill pair's pass).
pub(crate) fn unique_now(u: &UniqueKey, b: &mut FunctionBuilder<'_>, env: &Env) -> Option<Value> {
    use crate::ops::hashjoin::build::{DENSE, FLAGS, FORM, F_DUP, KIND, PHASE};
    if u.unique {
        return Some(b.bconst(true));
    }
    let r = crate::ops::hashjoin::build::region(b, env, Some(u.consumed?));
    let mut ok = b.bconst(true);
    for (off, mask, want) in [(gravity_abi::breaker::TABLE_KIND_OFF, u8::MAX, KIND), (PHASE, u8::MAX, 2), (FORM, u8::MAX, DENSE), (FLAGS, F_DUP, 0), (crate::ops::hashjoin::spill::build::SPILL, u8::MAX, 0)] {
        let v = b.load_zext(Type::I8, r, off as i32);
        let m = b.iconst(Type::I64, mask as i64);
        let v = b.int_bin(IntBinOp::And, v, m);
        let w = b.iconst(Type::I64, want as i64);
        let eq = b.icmp(IntCond::Eq, v, w);
        ok = b.int_bin(IntBinOp::And, ok, eq);
    }
    Some(ok)
}

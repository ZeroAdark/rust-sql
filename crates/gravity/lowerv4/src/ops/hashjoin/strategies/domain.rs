//! The domain filter, road `join.domain` (D3148): a keyed hash aggregate whose groups only a hash join above it reads,
//! on its group key, folds only the rows whose key the join's finished build holds. Budget 160 lines. Owner M1-JOIN.
//!
//! The shape: pipeline `k` ends in the keyed hash end of group `g`; pipeline `q`, the only reader of the group, emits it
//! and probes build slot `A` (built by pipeline `b < k`, so finished before `k` runs) at its first probe `j`, on a key
//! whose part 0 is group key `i`. A group whose key no build record holds never leaves that probe when it drops its
//! unmatched rows (`drops_unmatched`), so its rows need no fold: after every run of `b` the runner lays the build's part-0
//! keys as keyed.twopass's admitted set (exec's join.rtfilter protocol, the row-level form beside the granule filter:
//! one export path, one per-run lifecycle) and the hash end probes it on the group key before its upsert (twopass.rs
//! `Admit::probe`). Over part 0 alone the set admits a superset of the groups the join keeps, so another key part, the
//! join qual and a NULL key are still the probe's to decide. A build that spilled, holds op 14's error or outgrows the cap
//! admits every key (the reader gives no set). The rows a cut skips run nothing PostgreSQL would have run and could fail
//! on: effects `domain_cut_free`. A build a rerun set runs again filters only a `k` the set runs again too (D2856).
use gravity_abi::breaker::KeyKind;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::agg::GroupDecl;
use gravity_decompose::{Op, Source};
use gravity_program::protos::{Proto, ProtoKind};
use gravity_program::{LifeId, Rep};
use types_nodes::primnodes::OUTER_VAR;
use types_nodes::Node;

use crate::frame::px::Px;
use crate::graph::Gx;
use crate::ops::hashjoin::build::{DBITS, DIR, FAILED, FORM, HI, KIND, LO, PHASE};
use crate::ops::hashjoin::build_class::Side;
use crate::ops::hashjoin::rt::{rd, Rt};
use crate::ops::shared::twopass::Admit;
use crate::table::rec::GroupRec;

mod raw;
use raw::domain_keys;

/// The road's value: `admit` is the filter.
pub(crate) struct Domain;

crate::strategies! {
    /// A finished build's key set admits a keyed hash aggregate's rows on the group key the join above it probes.
    JOIN_DOMAIN = 128, "join.domain", JoinFold;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(JOIN_DOMAIN, &Domain)], ..crate::ops::NONE };

/// The admission of the keyed hash end of pipeline `k` (group `g`, record `rec`) by the build its only reader probes
/// first, when the shape above holds: the protocol word the runner lays the set behind, and the group key it is probed on.
pub(crate) fn admit(px: &mut Px<'_, '_>, k: usize, g: &GroupDecl<'_>, rec: &GroupRec<'_, '_>) -> Option<Admit> {
    if !crate::face::source::on(crate::admit::refusal::Variant::Strategy(JOIN_DOMAIN)) || rec.handle.is_some() || g.set.is_some() {
        return None;
    }
    let gx = px.gx;
    let (q, j, slot, b, i) = target(gx, k, g)?;
    let kd = &g.keys[i];
    let word_key = matches!(kd.kind, KeyKind::I32 | KeyKind::I64 | KeyKind::Date | KeyKind::Timestamp) && kd.image.is_raw();
    if !word_key || rec.key_words(i).1 != 1 || !matches!(crate::cell::key_cell_rep(gx, k, kd), Some(Rep::Word { .. })) {
        return None;
    }
    // D2856: a set that runs the build again must run `k` again too, so every run folds by its own run's keys
    if gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(b as u16)) && !r.pipelines.contains(&(k as u16))) {
        return None;
    }
    // selective by the planner's numbers (cost DOMAIN_RATIO; `PGRUST_GRAVITY_DOMAIN_RATIO` a test lever, 0 none)
    let ratio = gravity_driver::env::num::<u64>("PGRUST_GRAVITY_DOMAIN_RATIO").unwrap_or(gravity_program::cost::DOMAIN_RATIO);
    let rows = gx.pipelines()[b].desc.build_estimate();
    if ratio > 0 && rows.saturating_mul(ratio) > g.groups {
        return None;
    }
    if !crate::effects::domain_cut_free(gx, g, q, j) {
        return None;
    }
    let cap = if ratio > 0 { (g.groups / ratio).min(gravity_program::cost::DOMAIN_SET_MAX) } else { gravity_program::cost::DOMAIN_SET_MAX };
    let word = px.proto_words(1);
    px.declare_proto(Proto::new(ProtoKind::RtFilter, k as u16).peer(b as u16).slot(slot).words(word, 1).arg(cap.min(u32::MAX as u64)).reading(0, domain_keys));
    fires::fire("rtfilter.domain"); // shapecheck fire signal (D3148): a keyed hash end takes its join's build keys
    Some(Admit { word, key: Some(i), null_word: false, every_window: true })
}

/// The probe the groups of `g` (the keyed end of pipeline `k`) meet first: `(q, j, slot, b, i)`. `q` is the one pipeline
/// reading the group, as its source; `j` its first probe, every op before it a filter, gate or projection; `slot` / `b`
/// that probe's build slot and its producer, before `k`; `i` the group key the probe's key part 0 reads, an integer, date
/// or timestamp join key. The probe drops its unmatched rows, is no catalog probe, and its build runs before `k` on either
/// path: PostgreSQL's build-first one, or an outer-first one whose build side is quiet and over no recursive union (no
/// lazy build, D824, moves it after `k`).
fn target(gx: &Gx<'_, '_>, k: usize, g: &GroupDecl<'_>) -> Option<(usize, usize, u8, usize, usize)> {
    let ps = gx.pipelines();
    if g.producer != LifeId(k as u16) {
        return None;
    }
    let reads = |c: usize| c != k && ps[c].desc.consumes.iter().any(|&s| s == g.slot && gx.life_of(c, s) == Some(g.producer));
    let mut readers = (0..ps.len()).filter(|&c| reads(c));
    let q = readers.next()?;
    if readers.next().is_some() || q >= gx.graph.pipelines.len() || !matches!(ps[q].source, Source::Breaker { slot } if slot == g.slot) {
        return None;
    }
    let ops = &ps[q].ops;
    let j = ops.iter().position(|op| !matches!(op, Op::Filter(_) | Op::Gate(_) | Op::Project(_)))?;
    let Op::Join(JoinDesc::Probe { slot, jointype, keys, decls, outer_first, catalog: false, .. }) = &ops[j] else { return None };
    let b = gx.life_of(q, *slot)?.0 as usize;
    let safe_side = || crate::ops::hashjoin::build_class::side(gx, b) == Side::Safe && !crate::ops::hashjoin::build_class::endless(gx, b, q);
    if b >= k || !crate::ops::shared::scan::drops_unmatched(*jointype) || *outer_first && !safe_side() {
        return None;
    }
    let (Some(key), Some(d)) = (keys.iter().next(), decls.iter().next()) else { return None };
    let a = source_col(gx.facts, &ops[..j], key)?;
    let i = g.keys.iter().position(|kd| kd.resno == a)?;
    (d.key.kind == Some(g.keys[i].kind)).then_some((q, j, *slot, b, i))
}

/// The source column `key` reads (an `OUTER_VAR` through the projections in `ops`, the last one's entry first), through
/// relabels that keep the word (D3630: judged by base types, so a domain over oid read as int4 is no column of the key);
/// `None` for any other expression.
fn source_col(facts: &dyn gravity_decompose::catalog::Facts, ops: &[Op<'_>], key: Node<'_>) -> Option<i16> {
    fn strip<'n>(facts: &dyn gravity_decompose::catalog::Facts, n: Node<'n>) -> Node<'n> {
        let mut s = n.as_target_entry().map_or(n, |te| te.expr);
        while let Some(a) = gravity_decompose::expr::through_relabel_in(facts, s) {
            s = a;
        }
        s
    }
    let mut n = strip(facts, key);
    let mut projs = ops.iter().rev().filter_map(|op| if let Op::Project(l) = op { Some(*l) } else { None });
    loop {
        let v = n.as_var().filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0 && v.varattno > 0)?;
        match projs.next() {
            Some(l) => n = strip(facts, l.iter().nth(v.varattno as usize - 1)?),
            None => return Some(v.varattno),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key read through a projection lands on the source column its entry names; one past the projection's entries or
    /// a scan column does not.
    #[test]
    fn a_key_reads_its_source_column_through_the_projections() {
        use mcx::{Mcx, MemoryContext};
        use types_nodes::list::NodeList;
        use types_nodes::primnodes::{TargetEntry, Var};
        use gravity_decompose::catalog::NoFacts;
        let m: Mcx<'static> = Box::leak(Box::new(MemoryContext::new("domain"))).mcx(); // alloc: Cold
        let var = |varno: i32, a: i16| Node::mk(m, Var { varno, varattno: a, vartype: 23, vartypmod: -1, ..Default::default() }).unwrap();
        let te = |e: Node<'static>, resno: i16| Node::mk(m, TargetEntry { expr: e, resno, resname: None, ressortgroupref: 0, resorigtbl: 0, resorigcol: 0, resjunk: false }).unwrap();
        let list: &'static NodeList<'static> = Box::leak(Box::new(NodeList::from_slice(m, &[te(var(OUTER_VAR, 3), 1), te(var(OUTER_VAR, 1), 2)]).unwrap())); // alloc: Cold
        let ops = [Op::Project(list)];
        assert_eq!(source_col(&NoFacts, &ops, var(OUTER_VAR, 2)), Some(1), "entry 2 reads source column 1");
        assert_eq!(source_col(&NoFacts, &ops, var(OUTER_VAR, 1)), Some(3), "entry 1 reads source column 3");
        assert_eq!(source_col(&NoFacts, &[], var(OUTER_VAR, 2)), Some(2), "no projection: the source's own column");
        assert_eq!(source_col(&NoFacts, &ops, var(OUTER_VAR, 3)), None, "past the projection's entries");
        assert_eq!(source_col(&NoFacts, &[], var(1, 2)), None, "a scan Var is no emitted column");
    }
}

//! Road `key.dependent` (H10: group by a build key the build proved unique; L4 straight, L6 engaged) (cards/M6.md L4,
//! M6-plan §3.6; D3324 (b)). Owner M6-KEYED. Budget 190 lines.
//!
//! The road applies to a join whose fold or keyed step has a varlena key (`Applies::JoinFoldVarlena`, applies.rs).
//! STRAIGHT FORM (regress rules-062 and its kin): the group keyed by its full key. Decompose makes every column the grouping
//! key determines a key of its own (agg.rs `duplicate_keys` D416, `pk_keys` D1214), so every row of a group carries the same
//! bytes there and the group emits PG's value.
//! ENGAGED FORM (`find`, D3324 (b), the groupjoin argument of Moerkotte and Neumann, VLDB 2011, and late materialization):
//! a hashed group the probe pipeline ends in, one of whose keys `D` is an integer column of a build's row that is the
//! build's one join key (an INNER probe), and whose other text keys are columns of the same build row from code lanes of
//! the build's scan. Each joined row's build row is the one record whose key equals `D`, so where the build's keys are
//! unique the text keys are functions of `D` within a group: they leave the key image (0 words, their NULL flags kept)
//! and the record keeps the build row's code word, rendered through the build's dictionary only for the groups the emit
//! hands on (emit_key.rs `KeyOut::Code`'s `home`; a top over finals renders its kept groups alone). Uniqueness is read
//! at run time from the build's header (`unique_now`: laid out DENSE, no duplicate, not spilled), or is the planner's
//! (`inner_unique`); otherwise each row images the keys by their render (rep.code.hash), the groups PG's either way.
//! The projection after the probe hands the build row's code cells on as they are (step/project.rs `inner`).
//! Where join.payload_fp carries the column, the fallback's image is its Fp128 words (rep.code.hash `linked`).
//! `D` may also be the probe side's own join key column (`GROUP BY e.tenant_id` over `ON t.tenant_id = e.tenant_id`: every
//! joined row holds the build key there; dep_key.rs `outer_key`), and a top over the groups may order by keys that keep
//! their image beside finals (`ORDER BY sum(x) DESC, t.tenant_id`).
//! Declines: another group form, a two-pass or grouping-sets group, a streamed group, a top that orders by a dependent key
//! or anything but finals and image keys, a pair join.buildside may swap, an Arms run's build, a hoisted column, a key
//! derived from a column, a column an aggregate reads.
//! CARRIED FORM (`find`, D3324 R3, road key.carry; late materialization): where no key is a unique build key, the text
//! keys that are columns of one INNER probe's build row from code lanes of its scan keep that row's code word in the record
//! (`part << 32 | code`, the build's site dictionaries held for the statement) and render through the build's dictionary
//! only for the groups the emit hands on, where they rendered on every joined row into a copy (the value form). Their image
//! is their own (join.payload_fp's Fp128 words where the build carries them, else the render's), so the groups are PG's
//! and the table spills as any code-keyed one. The engaged form's machinery with a build that never skips (`UniqueKey`
//! with no region and not unique: `unique_now` answers `None`).
//! Registers: road `key.dependent` (`KEY_DEPENDENT`); `pgrust.gravity_road_off = 'key.dependent'` keeps the straight form;
//! road `key.carry` (`KEY_CARRY`), default off (road_off.rs `DEFAULT_OFF`): on under `pgrust.gravity_road_off = '+key.carry'`,
//! else the value form.
use gravity_abi::breaker::KeyKind;
use gravity_decompose::ops::shared::agg::{GroupDecl, GroupForm, NONE};
use gravity_decompose::Op;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::Value;
use gravity_program::Rep;
use types_nodes::primnodes::OUTER_VAR;

use crate::frame::env::Env;
use crate::frame::iface::UniqueKey;
use crate::frame::px::Px;
use crate::ops::Table;

crate::strategies! {
    /// Group by a build key the build proved unique at run time (a duplicate falls back to the full key); dependent
    /// columns ride as a locator and are fetched for output groups only.
    KEY_DEPENDENT = 85, "key.dependent", JoinFoldVarlena;
    /// D3324 R3: the text keys of one build row's code lanes keep that row's code and render at the emit (no unique key).
    /// Default off (road_off.rs `DEFAULT_OFF`): on under `+key.carry`.
    KEY_CARRY = 136, "key.carry", JoinFoldVarlena;
}

pub(crate) const ROWS: Table = Table { strategies: &[(KEY_DEPENDENT, &()), (KEY_CARRY, &())], ..crate::ops::NONE };

/// A group keyed through a build's join key (`find`): the build (`UniqueKey`, frame::iface; the carried form's never
/// skips) and the dependent keys (bit i: key i), each a column of that build's row its records keep as a code (`each`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Dep {
    pub u: UniqueKey,
    pub mask: u64,
}

impl Dep {
    pub(crate) fn has(&self, i: usize) -> bool {
        i < u64::BITS as usize && self.mask >> i & 1 == 1
    }
}

/// Group `g`'s engaged form (module doc), `None` where it declines.
pub(crate) fn find(px: &mut Px<'_, '_>, g: &GroupDecl<'_>) -> Option<Dep> {
    let gx = px.gx;
    let q = g.producer.0 as usize;
    let on = |px: &Px<'_, '_>, r| px.strategies.has(r) && crate::face::source::on(crate::admit::refusal::Variant::Strategy(r));
    let (road, carry) = (on(px, KEY_DEPENDENT), on(px, KEY_CARRY));
    // a top over the groups (runs.topk_groups) compares records' key words; a later sort reads the emitted cells' renders
    let topped = gx.drainers(g).first().is_some_and(|&k| gx.end_need(k) == crate::face::kinds::KindId::RunsTopkGroups);
    let streamed = gx.pipelines()[q].ops.iter().any(|op| matches!(op, Op::GroupRun(_)));
    // D4620: a cascade's base's records are rewritten key by key (a set's NULL columns): every key keeps its image
    if !(road || carry) || crate::ops::agg::cascade::base_group(gx, g) || g.form != GroupForm::Hashed || g.two_pass.is_some() || g.set.is_some() || g.keys.len() > u64::BITS as usize || !gx.group_at(q).is_some_and(|x| x.slot == g.slot) || streamed {
        return None;
    }
    for kd in g.keys.iter().filter(|kd| road && matches!(kd.kind, KeyKind::I32 | KeyKind::I64) && kd.resno > 0) {
        let Some(u) = crate::frame::iface::unique_build_key(px, q, kd.resno) else { continue };
        let mut mask = 0u64;
        each(px, g, &u, |i, _, _, _| mask |= 1 << i);
        // a top over the groups orders by finals, or (`ORDER BY sum(..) DESC, t.tenant_id LIMIT 20`) by keys that keep
        // their image too: never by a key that leaves it
        let kept = |r: i16| g.keys.iter().position(|k| k.resno == r).is_some_and(|i| i >= u64::BITS as usize || mask >> i & 1 == 0);
        if mask != 0 && (!topped || crate::frame::iface::top_reads_finals_and(gx, g, kept)) {
            fires::fire("key.dependent"); // shapecheck fire signal (D3324 (b)): text keys of a unique build key's row leave the image
            return Some(Dep { u, mask });
        }
    }
    // the carried form: the first text key's build row, with every key of that row's code lanes (one build per group)
    for kd in g.keys.iter().filter(|kd| carry && kd.kind == KeyKind::Fp128 && kd.resno > 0 && (!topped || crate::frame::iface::top_reads_finals(gx, g))) {
        let Some(u) = crate::frame::iface::build_row(px, q, kd.resno) else { continue };
        let mut mask = 0u64;
        each(px, g, &u, |i, _, _, _| mask |= 1 << i);
        if mask != 0 {
            fires::fire("key.carry"); // shapecheck fire signal (D3324 R3): a build row's text keys keep its code, rendered at the emit
            return Some(Dep { u, mask });
        }
    }
    None
}

/// Each key of `g` that is a text column of `u`'s build row the build keeps as a code (frame::iface `build_code_col`), with
/// its INNER_VAR attno, its column in the producer's row and its code rep; a key an aggregate also reads is not one.
pub(crate) fn each(px: &mut Px<'_, '_>, g: &GroupDecl<'_>, u: &UniqueKey, mut f: impl FnMut(usize, i16, i16, Rep)) {
    let (gx, q) = (px.gx, g.producer.0 as usize);
    for (i, k) in g.keys.iter().enumerate().take(u64::BITS as usize) {
        if k.kind != KeyKind::Fp128 || k.resno <= 0 || k.rep == NONE || !gx.facts.coll(k.coll).bytes_eq || agg_reads(g, k.resno) {
            continue;
        }
        let bp = crate::dict::coderep::sk_bpchar(gx, k.tf) != 0;
        if let Some((attno, rep)) = crate::frame::iface::build_code_col(px, q, u, k.resno, bp) {
            f(i, attno, k.resno, rep);
        }
    }
}

/// Whether an aggregate or its FILTER reads column `col` of the group's row (it would read the build row's code, not text).
fn agg_reads(g: &GroupDecl<'_>, col: i16) -> bool {
    g.aggs.iter().flat_map(|a| [Some(a.node), a.filter]).flatten().any(|n| {
        let mut hit = false;
        let known = gravity_decompose::expr::walk(n, &mut |x| {
            hit |= x.as_var().is_some_and(|v| v.varno == OUTER_VAR && v.varlevelsup == 0 && v.varattno == col);
            true
        });
        hit || !known
    })
}

/// Whether INNER_VAR column `a` is one projection op `j` of pipeline `k` hands on as the build row's code cell: a
/// dependent key's column, where the op before `j` is the probe `find` names.
pub(crate) fn forwards(px: &mut Px<'_, '_>, k: usize, j: usize, a: i16) -> bool {
    let Some(g) = px.gx.group_at(k) else { return false };
    let Some(i) = px.gx.pipelines()[k].ops[..j].iter().rposition(|op| !matches!(op, Op::Filter(_))) else { return false };
    let Some(d) = find(px, g).filter(|d| d.u.j == i) else { return false };
    let mut hit = false;
    each(px, g, &d.u, |i, attno, _, _| hit |= d.has(i) && attno == a);
    hit
}

/// Whether the build is unique on its key now (`Bool`, frame::iface `build_unique_now`); `None` where the form never skips.
pub(crate) fn unique_now(d: &Dep, b: &mut FunctionBuilder<'_>, env: &Env) -> Option<Value> {
    crate::frame::iface::build_unique_now(&d.u, b, env)
}

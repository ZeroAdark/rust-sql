//! The parted form of a fronted group under a HAVING count floor (V4R-105, lever L-DEFERMIN, cards/M2.md "the count-walk
//! front"). A group the per-code front takes (keyed_codefront_percode, keyed_countwalk) whose HAVING requires
//! `count(*) > K` / `>= K` (keyed_twopass_having `floor`) keys its records by `(image, part)`: the part word after the
//! image, which alone routes (`partitioned_by`: a group's parts meet in one owner partition). Each `min(text)` keeps its
//! code word `part << 32 | code` (the front folds it as the key's representative, `Fold::Rep`; the row path through
//! `CodeSlot`): no render and no copy per part and key. The owner is kernels parthaving (a group's parts summed, their
//! minima rendered and compared by bytes for the groups at the floor alone, only those kept), K read from its bound word per
//! execution, never folded; the emit renders each survivor's minimum once (`finish`, emit_group `finals`) and its own floor
//! still decides. Hooks: keyed_hashagg `make` / `GroupRec::new` / `image_of` / `pipeline_desc`, emit_group `finals`,
//! fold_class_minmax_code's dispatch, host_abi v4 m1_countwalk `image`. Budget 270 lines (files-extra.tsv, +25 % gate).
//! Owner M2-COUNTWALK.
use gravity_abi::breaker::{KeyKind, OwnerFns, RecordLayout, SlotKind, LAYOUT_IMPLICIT_COUNT, MINCODE_NONE, ORDER_NONE};
use gravity_decompose::ops::shared::agg::{AggDecl, FinalForm, GroupDecl, Register};
use gravity_decompose::Op;
use gravity_expr::emit::CellValid;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type, Value};
use gravity_kernels::ops::sort::topn::parthaving::PartHavingSpec;
use gravity_program::{NumTypmod, OwnerDesc, Rep, MAX_REPS, REP_NONE};
use types_nodes::{primnodes::OUTER_VAR, Node, NodeList};
use mcx::{CollectIn, GrowIn, PgBox, PgVec};

use crate::table::emit_group::RepSrc;
use crate::table::rec::{Agg, GroupRec};
use crate::cell::{Cell, Nullable};
use crate::face::end::EndMech;
use crate::face::fold::{Acc, AccShape, FoldClass};
use crate::face::kinds::{Face, KindId};
use crate::frame::env::Env;
use crate::frame::px::Px;
use crate::admit::refusal::Reason;
use crate::rep::CellRef;

/// Face::Fold's make-time state (the deferral is the minima's fold; Face::Keyed's slot holds keyed_twopass's homes): each
/// parted group `(slot, graph producer)`, its deferred minima `(slot, code lane)` and the owner's floor word (`owner_fns`).
#[derive(Default)]
struct DeferExt {
    groups: Vec<Filed>,
}
crate::frame::px::ext_g!(DeferExt);
type Filed = (u8, u16, [(u8, u8); MAX_REPS], u32);

fn filed<'x>(px: &'x Px<'_, '_>, g: &GroupDecl<'_>) -> Option<&'x Filed> {
    px.ext::<DeferExt>(Face::Fold)?.groups.iter().find(|e| e.0 == g.slot && e.1 == g.producer.0)
}

/// Pipeline `k`'s end over group `g`, which the per-code front takes (`rec` its one-pass record), in the parted form (the
/// module doc); `None` keeps the one-pass form: a `max(text)`, no `min(text)`, no count floor, a count-ordered top, a top
/// over a deferred minimum (the group sort compares images, not codes), a top with D472 order slots, a parted record the
/// front declines.
pub(crate) fn parted<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, g: &'g GroupDecl<'p>, rec: &GroupRec<'g, 'p>) -> Option<PgBox<'g, dyn EndMech + 'g>> {
    let gx = px.gx;
    let Some(Some(RepSrc { rep: Rep::Code { dict }, .. })) = rec.reps.first().filter(|_| rec.dep.is_none()) else { return None };
    let text = |a: &&Agg<'_>| crate::props::class_kind(&a.d) == KindId::ClassMinmaxCode && a.d.final_ == FinalForm::Rendered;
    if rec.aggs.iter().filter(text).any(|a| a.d.reg != Register::MinCode) {
        return None;
    }
    let mins: PgVec<'_, &Agg<'_>> = rec.aggs.iter().filter(text).collect_in(px.mcx());
    let q = gx.drainers(g).first().copied()?;
    let slotted = !crate::ops::sort::topn::order_slot::kinds(gx, g).is_empty(); // D472's order slots keep the one-pass form
    if mins.is_empty() || mins.len() > MAX_REPS || slotted || crate::ops::sort::topn::lower::count_top_of(px, k).is_some() || sorts_by(px, q, &mins[..]) {
        return None;
    }
    // the form prunes by a count floor (`floor`: any lower bound a HAVING puts on any count final, >, >= or =); under a
    // HAVING with none every group survives the owner, the form only adds the parts' collapse, and nothing measured wins
    // PSP2-43: the owner reads K's word as an int, never a numeric's pointer
    let f = crate::ops::sort::topn::strategies::twopass_having::floor(px, q, rec).filter(|f| !f.num && f.count && !f.below)?;
    let count = (0..rec.layout.nslots()).find(|&j| rec.layout.slot_off(j) as i32 == f.off)?;
    let mut reps = [(REP_NONE, 0u8); MAX_REPS];
    for (r, a) in reps.iter_mut().zip(mins.iter()) {
        *r = (u8::try_from(a.d.slot).ok()?, dict.lane); // the owner packs a representative slot in a byte
    }
    let word = f.word as u32 | (f.gt as u32) << 16 | (f.int4 as u32) << 17 | (count as u32) << 24;
    px.ext_mut::<DeferExt>(Face::Fold).groups.push((g.slot, g.producer.0, reps, word)); // alloc: Program
    let rec2 = GroupRec::new(px, g);
    match crate::dict::strategies::codefront_percode::plan(px, k, &rec2) {
        Some(pc) if rec2.layout.partition_keys() < rec2.layout.nkeys() => Some(crate::frame::iface::group_end_of(px, k, rec2, None, None, Some(pc))),
        _ => {
            px.ext_mut::<DeferExt>(Face::Fold).groups.pop();
            None
        }
    }
}

/// Whether pipeline `q`'s top over the groups orders by an expression reading one of `mins` (read through the group's
/// projection; an expression the walker does not know counts as one).
fn sorts_by(px: &Px<'_, '_>, q: usize, mins: &[&Agg<'_>]) -> bool {
    let p = &px.gx.pipelines()[q];
    let Some(j) = p.ops.iter().position(|op| matches!(op, Op::Sort(_))) else { return false };
    let Op::Sort(sd) = &p.ops[j] else { return false };
    let project = p.ops[..j].iter().rev().find_map(|op| if let Op::Project(l) = op { Some(*l) } else { None });
    fn entry<'p>(l: &NodeList<'p>, r: i16) -> Option<Node<'p>> { l.iter().filter_map(|n| n.as_target_entry()).find(|te| te.resno == r).map(|te| te.expr) }
    sd.keys.iter().any(|key| {
        let Some(mut e) = entry(sd.tlist, key.resno) else { return true };
        if let (Some(v), Some(l)) = (e.as_var().filter(|v| v.varno == OUTER_VAR), project) {
            let Some(x) = entry(l, v.varattno) else { return true };
            e = x;
        }
        let mut hit = false;
        let known = gravity_decompose::expr::walk(e, &mut |n| { hit |= n.as_aggref().is_some_and(|r| mins.iter().any(|a| a.d.aggno as i32 == r.aggno)); true });
        hit || !known
    })
}

/// Group `g`'s record layout `l` (keyed_hashagg `GroupRec::new`), parted when `parted` filed the group: each deferred
/// minimum's slot a `MinCode` again (`rekind` undone: the front keeps the code word), then the part word (`I64`) after
/// the image, the layout partitioned by the image's words alone.
pub(crate) fn parted_layout(px: &Px<'_, '_>, g: &GroupDecl<'_>, l: RecordLayout) -> RecordLayout {
    let Some(e) = filed(px, g) else { return l };
    let mut keys = mcx::to_vec_in(&l.key_kinds()[..], px.mcx());
    keys.push_in(KeyKind::I64);
    let implicit = l.flags & LAYOUT_IMPLICIT_COUNT != 0;
    let mut slots = mcx::to_vec_in(&l.slot_kinds()[..], px.mcx());
    slots.truncate(slots.len() - implicit as usize);
    e.2.iter().filter(|m| m.0 != REP_NONE).for_each(|&(s, _)| slots[s as usize] = SlotKind::MinCode);
    let sentinel = (!implicit).then(|| l.sentinel_slot());
    gravity_program::record_layout_at(px.arena, &keys[..], &slots[..], l.has_validity(), sentinel).partitioned_by(l.nkeys() as u8)
}

/// Aggregate `d`'s class in a record of layout `l`: the deferred minimum's (`CodeSlot`) for a code `min` whose slot a
/// parted layout keeps a `MinCode`; else `class_of`'s.
pub(crate) fn class_in(l: &RecordLayout, node: &i32, d: &AggDecl<'_>, trees: bool, mcx: mcx::Mcx<'_>) -> &'static dyn FoldClass {
    let parted = l.partition_keys() < l.nkeys() && l.slot_kind(d.slot as usize) == SlotKind::MinCode && d.reg == Register::MinCode;
    if parted && crate::props::class_kind(d) == KindId::ClassMinmaxCode { &CODE_SLOT } else { crate::frame::iface::fold_class_in(node, d, trees, mcx) }
}

/// The owner descriptor of a parted group's end (keyed_hashagg `pipeline_desc`): parthaving's, bound per execution by
/// `owner_fns`; `o` for any other group.
pub(crate) fn owner(px: &Px<'_, '_>, rec: &GroupRec<'_, '_>, o: OwnerDesc) -> OwnerDesc {
    match filed(px, rec.g) {
        Some(&(_, _, reps, word)) => OwnerDesc { ctor_desc: Some(owner_fns), order_slot: ORDER_NONE, topk: word, reps, ..o },
        None => o,
    }
}

/// The parted form's `OwnerCtorDesc`: parthaving over the record under this execution's floor, K read from its bound word
/// (`topk`: the word, bit 16 `>`, bit 17 an int4 K, bits 24.. the count slot); each deferred minimum `(slot, lane)` in
/// `reps`. A NULL K keeps what its word says: the emit's floor (keyed_twopass_having `keep`) keeps no group of it.
fn owner_fns(d: &OwnerDesc, words: &[u64]) -> OwnerFns {
    let (w, gt, int4, count) = (d.topk & 0xffff, d.topk >> 16 & 1 != 0, d.topk >> 17 & 1 != 0, (d.topk >> 24) as u8);
    let k = words.get(w as usize).map_or(0, |&x| if int4 { x as i32 as i64 } else { x as i64 });
    let floor = if gt { k.saturating_add(1) } else { k };
    gravity_kernels::ops::sort::topn::parthaving::owner_fns(&PartHavingSpec { layout: d.layout, mins: d.reps, count_slot: count, bits: 0, floor })
}

/// The group emit's final of aggregate `a` over the record at `at` (emit_group `finals`): a deferred minimum's code word
/// rendered through its source's dictionary (a `Rep::Code` record cell, the rep face's datum, as a text key renders), NULL
/// when unset; any other aggregate's `FoldClass::finish`.
pub(crate) fn finish(r: &GroupRec<'_, '_>, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &Agg<'_>, at: Value) -> (Value, Value) {
    let (Some(Some(src)), true) = (r.reps.first(), deferred(a.class)) else { return a.class.finish(b, env, &a.d, r.slot(at, a.d.slot)) };
    let w = b.load(Type::I64, at, r.layout.slot_off(a.d.slot as usize) as i32);
    let set = crate::ops::shared::slots::slot_ok(b, env, at, crate::ops::shared::slots::slot_valid(&r.layout, a.d.slot));
    let none = b.const_raw(Type::I64, MINCODE_NONE);
    let some = b.icmp(IntCond::Ne, w, none);
    let (ok, t) = (b.int_bin(IntBinOp::And, set, some), b.bconst(true));
    let null = b.int_bin(IntBinOp::Xor, ok, t);
    let cell = Cell::of_record(src.rep, src.typid, NumTypmod::NONE, r.home, a.d.slot as u16, Nullable::Maybe);
    let mut cx = env.lx(b, r.node);
    let v = crate::rep::datum(&mut cx, CellRef { cell: &cell, val: w, null: Some(null) }, &r.tfs[0]);
    env.memo = cx.memo;
    v
}

/// The deferred minimum's class: a record slot keeps the word `part << 32 | code` of its best row (the record's part is
/// every row's, so codes compare as bytes under the byte-order collation the front requires); registers are
/// class.minmax_code's.
struct CodeSlot(#[allow(dead_code)] u8);
static CODE_SLOT: CodeSlot = CodeSlot(0);

/// Whether `class` is the deferred minimum's, by type (`FoldClass::deferred_code`, V4R-252). D720: never by address. The
/// zero-sized `WIDE` (class.num_wide) sat at CODE_SLOT's address in one link, so its sum / avg rendered through the
/// dictionary. D2916 swept the rest; lint rule R13 refuses a new address compare of a static, a trait object or a function.
pub(crate) fn deferred(class: &dyn FoldClass) -> bool {
    class.deferred_code()
}

/// The slot takes `word` where `when` holds and the slot is unset or `rank` is at most its code (a tie takes the row).
#[allow(clippy::too_many_arguments)]
fn slot_min(b: &mut FunctionBuilder<'_>, env: &Env, base: Value, off: i32, valid: CellValid, word: Value, rank: Value, when: Value) {
    let cur = b.load(Type::I64, base, off);
    let m = b.iconst(Type::I64, u32::MAX as i64);
    let (code, set, t) = (b.int_bin(IntBinOp::And, cur, m), crate::ops::shared::slots::slot_ok(b, env, base, valid), b.bconst(true));
    let (unset, lower) = (b.int_bin(IntBinOp::Xor, set, t), b.icmp(IntCond::Ule, rank, code));
    let take = b.int_bin(IntBinOp::Or, unset, lower);
    let take = b.int_bin(IntBinOp::And, take, when);
    let w = b.select(take, word, cur);
    b.store(base, off, w);
    crate::ops::shared::slots::mark(b, env, base, valid, take);
}

impl FoldClass for CodeSlot {
    fn shape(&self, a: &AggDecl<'_>) -> AccShape {
        crate::dict::minmax_code::code().shape(a)
    }

    fn deferred_code(&self) -> bool {
        true
    }

    /// A slot opens unset (`MINCODE_NONE`, abi `Rec::valid`'s rule over no validity word).
    fn init(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>) {
        let Acc::Slot { base, off, .. } = acc else { return crate::dict::minmax_code::code().init(b, env, a, acc) };
        let none = b.const_raw(Type::I64, MINCODE_NONE);
        b.store(base, off, none);
    }

    /// A row into the slot: its code ranked under the aggregate's collation (key.coll refuses one not ordering by bytes).
    fn step(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>, args: &[CellRef<'_>]) {
        let Acc::Slot { base, off, valid } = acc else { return crate::dict::minmax_code::code().step(b, env, a, acc, args) };
        let [c] = args else { crate::refuse!(env.node, Reason::Bug { payload: "a deferred minimum over other than one argument" }) };
        let t = b.bconst(true);
        let ok = c.null.map_or(t, |n| b.int_bin(IntBinOp::Xor, n, t));
        let mut cx = env.lx(b, env.node);
        let (rank, word) = (crate::rep::code::order::rank(&mut cx, *c, a.coll), crate::rep::code::datum::word(&mut cx, *c));
        env.memo = cx.memo;
        slot_min(b, env, base, off, valid, word, rank, ok);
    }

    /// A window's carry (`[word, where, n]`) or another record's slot of the same `(image, part)` into the slot.
    fn merge(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, into: Acc<'_>, from: Acc<'_>) {
        let Acc::Slot { base, off, valid } = into else { return crate::dict::minmax_code::code().merge(b, env, a, into, from) };
        let (word, when) = match from {
            Acc::Regs(f) => (f[0], b.icmp(IntCond::Ne, f[2], env.zero)),
            Acc::Slot { base: fb, off: fo, valid: fv } => {
                let w = b.load(Type::I64, fb, fo);
                let none = b.const_raw(Type::I64, MINCODE_NONE);
                let (set, some) = (crate::ops::shared::slots::slot_ok(b, env, fb, fv), b.icmp(IntCond::Ne, w, none));
                (w, b.int_bin(IntBinOp::And, set, some))
            }
        };
        let m = b.iconst(Type::I64, u32::MAX as i64);
        let rank = b.int_bin(IntBinOp::And, word, m);
        slot_min(b, env, base, off, valid, word, rank, when);
    }

    /// The registers' render; a slot's word is the group emit's to render (`finish`: it knows the dictionary).
    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>) -> (Value, Value) {
        if let Acc::Slot { .. } = acc {
            crate::refuse!(env.node, Reason::Bug { payload: "a deferred minimum's slot finished outside the group emit" });
        }
        crate::dict::minmax_code::code().finish(b, env, a, acc)
    }
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::{deferred, FoldClass, CODE_SLOT};
    use crate::face::fold::ClassImpl;
    use crate::face::kinds::KindId;

    /// D720: every registered class, and every zero-sized one (class.num_wide's `WIDE` among them) placed at CODE_SLOT's
    /// address as a link may place it, is not the deferred minimum; CODE_SLOT is.
    #[test]
    fn d720_a_zero_sized_class_at_code_slot_s_address_is_not_the_deferred_minimum() {
        assert!(deferred(&CODE_SLOT), "CODE_SLOT is the deferred minimum");
        let at = core::ptr::addr_of!(CODE_SLOT).addr();
        let rows = crate::face::kinds::KINDS.iter().flat_map(|d| crate::ops::kind_rows(d.id).iter().map(move |m| (d.id, *m)));
        let (mut moved, mut wide) = (0, false);
        for (k, c) in rows.filter_map(|(k, m)| m.downcast_ref::<ClassImpl>().map(|c| (k, c.0))) {
            assert!(!deferred(c), "{k:?}: a registered class is not the deferred minimum");
            if core::mem::size_of_val(c) != 0 || at % core::mem::align_of_val(c) != 0 {
                continue;
            }
            // SAFETY: a zero-sized value is valid at any non-null address aligned for it (no byte is read)
            let c: &dyn FoldClass = unsafe { &*(c as *const dyn FoldClass).with_addr(at) };
            assert!(!deferred(c), "{k:?}: a zero-sized class at CODE_SLOT's address is not the deferred minimum");
            (moved, wide) = (moved + 1, wide || k == KindId::ClassNumWide);
        }
        assert!(moved > 0 && wide, "class.num_wide's WIDE is zero-sized and was placed at CODE_SLOT ({moved} placed)");
    }
}

/// A parted record's image is the row's key image, then the window's part ordinal: the u32 at this byte offset of the
/// context (the driving site header's `PART_ID`), which the image's build loads after its flags words (rec.rs `image_of`).
pub(crate) fn part_at(env: &Env, node: i32) -> (Value, i32) {
    let Some(s) = env.site else { crate::refuse!(node, Reason::Bug { payload: "a parted record without a driving site" }) };
    (env.ctx, (gravity_abi::ctx::site_off(s) + gravity_abi::site::site::PART_ID) as i32)
}

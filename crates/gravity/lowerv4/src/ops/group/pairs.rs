//! Kind `keyed.distinct_grouped` (cards/M3.md L6): `agg(DISTINCT x)` per group, the other aggregates riding. Owner
//! M3-KEYED. Budget 450 lines (files.tsv, +25 % gate).
//!
//! The PAIR record `[group keys.., x] -> riders` is M1's keyed fold: keyed.hashagg's end over the group decompose declared
//! (GroupForm::Distinct: the argument is its resno-0 key, after the group keys), each row folding its riders into its
//! pair's record (a NULL argument is a pair too, so the riders count its rows). This file's end is that end with the
//! owner phase replaced: `OwnerDesc::distinct` over the pair (exec owner.rs `owner_fns_dense`: the two-round owner
//! re-sinks every deduped pair by its group words and folds the group's accumulator: `[sentinel, pairs]` then the riders
//! merged, the pairs counted where the argument's NULL flag is clear). The emit (keyed.emit_group, and runs.topk_groups'
//! drain) reads the accumulator through `GroupRec::emit_view` (V4R-131): a `count(DISTINCT)` final is the pair count.
//!
//! D500: a DISTINCT fold (`sum / avg / min / max(DISTINCT x)` per group, `PairFold`) is an accumulator slot after the
//! pair's (`OwnerDesc::accumulator_folded`: its plain register's slot kind, validity on: an extremum starts unset, a group of NULL arguments
//! alone folds nothing and emits NULL) that the owner folds each counted pair's integer argument word into
//! (`OwnerDesc::arg_fold`, at most two); the emit reads it as the plain aggregate's slot (keyed/pairs_view.rs, M1's emit seam).
//! D500: a nullable group key: the image's flags word follows the argument, so the owner's group-flags form keys the
//! accumulator by the group words and that word with the argument's bit cleared (`OwnerDesc::group_flags`).
//! Whether a key is nullable is keyed_emit_group::key_nullable's alone (it built the table's image): this file reads
//! the answer from the pair record (a flags word after the argument), never from a rule of its own (D521).
//! Refused by name (Shape): a fold over a non-integer argument, a third fold, a nullable group key past the 8-key tier,
//! a FILTER beside a second DISTINCT argument (the pair FILTER itself: V4R-367, hashagg_rec.rs `pair_filter`), groups whose key order
//! the plan reads (D530: a sorted GroupAggregate with no Sort above; the owner emits in its partitions' order). A second
//! DISTINCT argument (D536: one pair set per group) refuses NotBuilt M6 keyed.distinct_multi (V4R-183).
//! G4: `count(DISTINCT x)` without GROUP BY stays keyed.distinct_plain (M1).
//! V4R-194 (v2 agg/pairs.rs's rule): one raw integer column as the group key names the owner's dense key
//! (`OwnerDesc::dense_key`), its domain the Facts protocol's DenseLo / DenseHi on the key's lane: hosts `part_extrema`
//! over this snapshot's part list (every visible row's part carries its exact extrema; deletes only widen), never the
//! newest generation's footer. exec owner.rs then takes the dense round 1 where `dense_form` holds and the span fits.
use gravity_abi::breaker::{KeyKind, SlotKind, ORDER_NONE};
use gravity_decompose::ops::shared::agg::fold_of;
use gravity_decompose::ops::shared::agg::{AggDecl, FinalForm, GroupDecl, NONE as NO_SLOT};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, Value};
use gravity_program::protos::{Fact, Proto, ProtoKind};
use gravity_program::{LaneId, OwnerDesc, Pipeline, WordsDesc, MAX_REPS, REP_NONE};
use mcx::{CollectIn, Mcx, PgBox, PgVec};

use crate::face::end::{EndMech, SinkKind};
use crate::face::kinds::KindId;
use crate::frame::env::Env;
use crate::frame::frame::{EndMaker, Win, WordWin};
use crate::frame::open::OpenWords;
use crate::frame::px::{Px, DRAFT_SITE};
use crate::frame::row::{Carry, Mask, RowVals};
use crate::frame::stop::Exits;
use crate::graph::Gx;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::ops::Table;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::KeyedDistinctGrouped, &EndMaker(make))], ..crate::ops::NONE };

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

/// The pair form's facts: the group keys' image words, the argument key's index (its flag `breaker::flag_at`), the plan's
/// group estimate.
struct Pair<'g> {
    group_words: usize,
    arg_words: usize,
    arg: u8,
    groups: u32,
    folds: [Option<SlotKind>; 3],
    node: i32,
    /// CAP-28: the group keys' image kinds (`coderep::key_kinds`, as pairs_view.rs lays the emit's record past one flags word).
    group_kinds: PgVec<'g, (KeyKind, u8)>,
    /// V4R-194: the dense key's attribute and the first of its two Facts words (DenseLo, DenseHi).
    dense: Option<(i16, u16)>,
}

/// Each DISTINCT fold of `g` with its slot kind (`fold_of`'s plain register's, the kinds exec `owner.rs::fold_arg` folds).
fn folds<'a, 'p>(g: &'a GroupDecl<'p>) -> impl Iterator<Item = (&'a AggDecl<'p>, Option<SlotKind>)> {
    let kind = |d: &AggDecl<'_>| fold_of(d.class, d.fnoid, false, false).1.abi().filter(|k| matches!(k, SlotKind::SumI64 | SlotKind::SumI128 | SlotKind::AvgI128 | SlotKind::MinI64 | SlotKind::MaxI64));
    g.aggs.iter().filter(|d| d.final_ == FinalForm::PairFold).map(move |d| (d, kind(d)))
}

/// D536: whether group `g` has a DISTINCT aggregate past the first argument (no record slot: keyed.distinct_multi's).
fn multi(g: &GroupDecl<'_>) -> bool {
    g.aggs.iter().any(|d| d.final_ == FinalForm::Distinct && d.slot == NO_SLOT)
}

fn pair_of<'g>(gx: &Gx<'g, '_>, g: &GroupDecl<'_>, node: i32) -> Pair<'g> {
    let mut fs = [None; 3];
    for (i, (_, k)) in folds(g).enumerate() {
        match (fs.get_mut(i), k) {
            (None, _) => shape(node, "a fourth DISTINCT fold per group (the owner folds three argument slots)"),
            (_, None) => shape(node, "a DISTINCT fold whose slot the owner does not fold"),
            (Some(f), k) => *f = k,
        }
    }
    let producer = g.producer.0 as usize;
    let Some(arg) = g.keys.iter().position(|k| k.resno == 0) else { shape(node, "a grouped DISTINCT without its argument key") };
    if arg + 1 != g.keys.len() {
        shape(node, "a grouped DISTINCT whose argument is not its last key");
    }
    // the record's key words (face/keyimg.rs record_words: a key under a nondeterministic collation, a narrow numeric over a
    // varlena cell, is two Fp128 words), never decompose's alone
    let kinds: PgVec<'_, _> = g.keys.iter().map(|k| crate::table::keyimg::record_words(gx, producer, k)).collect_in(gx.mcx);
    if fs[0].is_some() && !matches!(kinds[arg].0, KeyKind::I32 | KeyKind::I64) {
        shape(node, "a DISTINCT fold over an argument that is not an integer word");
    }
    let group_words = kinds[..arg].iter().map(|k| k.1 as usize).sum();
    let group_kinds = crate::dict::coderep::key_kinds(gx, producer, &g.keys[..arg]);
    Pair { group_words, arg_words: kinds[arg].1 as usize, arg: arg as u8, groups: g.groups.min(u32::MAX as u64) as u32, folds: fs, node, group_kinds, dense: None }
}

/// V4R-194: the one group key a raw integer column of pipeline `k`'s scan (an int, or a date / timestamp: a signed word
/// of days / microseconds, its lane extrema the same parts' answer): its Facts words declared (DenseLo, DenseHi of its
/// lane, from the snapshot's parts; `(1, 0)` none, as on a heap site), `(attno, first word)`. The keys after it are the
/// DISTINCT arguments (one, or a tagged form's several: pairs_multi.rs).
pub(crate) fn dense_of(px: &mut Px<'_, '_>, g: &GroupDecl<'_>) -> Option<(i16, u16)> {
    let [key, args @ ..] = &g.keys[..] else { return None };
    if args.is_empty() || args.iter().any(|a| a.resno != 0) || key.words != 1 || !matches!(key.kind, KeyKind::I32 | KeyKind::I64 | KeyKind::Date | KeyKind::Timestamp) {
        return None;
    }
    let v = key.expr.node().as_var()?;
    let k = g.producer.0 as usize;
    let (scanrelid, _, site) = px.gx.pipelines()[k].source.relation()?;
    let lane = site.lanes.iter().position(|l| l.id == LaneId::Attr(v.varattno)).filter(|_| v.varno == scanrelid as i32 && v.varattno > 0)?;
    let lane = u8::try_from(lane).ok()?;
    let mut base = px.proto_words(2);
    if base == 0 {
        base = px.proto_words(2);
    }
    let base = u16::try_from(base).ok().filter(|&b| b < WordsDesc::NONE - 2)?;
    let arg = 1u64 << Fact::DenseLo as u8 | 1 << Fact::DenseHi as u8;
    px.declare_proto(Proto::new(ProtoKind::Facts, k as u16).words(base, 2).site(DRAFT_SITE).slot(lane).arg(arg));
    Some((v.varattno, base))
}

/// The distinct owner over `pair` (the table's record, `pipeline_desc`'s owner layout): the words after the group words
/// are the pair's DISTINCT words, the argument's image and, where keyimg appended them (some key nullable), the flags
/// words (one per 64 keys, CAP-28), whose argument bit the owner reads as NULL (`null_arg`: the pair counts no pair). With
/// a nullable group key the group keys' flags words are the accumulator's last keys instead, the argument's bit cleared
/// where it shares one (`group_flag_words`: NULL and 0 stay apart), as the emit's view lays them (pairs_view.rs).
fn owner(arena: gravity_program::ArenaRef, pair: gravity_abi::breaker::RecordLayout, p: &Pair<'_>, top: Option<&OwnerDesc>, mcx: Mcx<'_>) -> OwnerDesc {
    // a nullable group key: the table's record carries the flags word after the argument (keyimg appends it where
    // keyed_emit_group::key_nullable calls a key nullable; dense.range's x % c / x / c over a NOT NULL column is not: D521),
    // so the record decides, never a second copy of the NULL rule
    let flags = pair.nkeys() > p.group_words + p.arg_words;
    // the owner names the pair's flags word by a byte (`null_arg`, NULL_ARG_NONE = 255) and the group-flags form builds its
    // accumulator keys in a 255-word buffer (exec owner.rs `ACC_KEYS_MAX`), while a record runs past 255 words (agg CAP-32).
    // D3374: regroup.rs `past_owner_words` re-declares such a group host-sorted first; one it could not (`blocked`) ends here
    if pair.nkeys() > u8::MAX as usize {
        shape(p.node, "a grouped DISTINCT whose pair runs past 255 record words (the owner's word ids are bytes)");
    }
    let words = if flags { p.arg_words } else { pair.nkeys() - p.group_words };
    let fs: PgVec<'_, SlotKind> = p.folds.iter().flatten().copied().collect_in(mcx);
    let (mut acc, first) = OwnerDesc::accumulator_folded(arena, &pair, words as u8, &fs[..], flags);
    // CAP-28: past 64 keys the accumulator's keys are the group keys' image words then their own flags words, one per 64
    // group keys, laid out as the emit's view reads them (pairs_view.rs `view`: `image_layout` over the group keys)
    let nflags = pair.nkeys() - p.group_words - p.arg_words;
    if flags && nflags > 1 {
        acc = crate::dict::coderep::image_layout(arena, &acc, &p.group_kinds, true, &[], acc.has_validity(), mcx);
    }
    // D563: a count(DISTINCT)-ordered top's count form (runs/owner_count.rs, its slot, order word and representatives the
    // accumulator's): round 2 keeps each owner's n best and the emit gathers only those (exec owner.rs distinct_reduce2,
    // order_slot2; or the one-round fold, hosts distinct_fold.rs)
    let (slot, n, reps, order) = top.map_or((ORDER_NONE, 0, [(REP_NONE, 0); MAX_REPS], 0), |t| (t.order_slot, t.topk, t.reps, t.order));
    let mut d = OwnerDesc::distinct(pair, acc, (pair.nkeys() - acc.nkeys()) as u8, p.groups, slot, n, reps);
    if order != 0 {
        d = d.ordered_by(order);
    }
    for i in (0..fs.len()).map(|i| first + i as u8) {
        d = d.arg_fold(i);
    }
    // the argument's flag: bit arg % 64 of flags word arg / 64 (`breaker::flag_at`), the pair's last word (it is the last key)
    let (fw, bit) = gravity_abi::breaker::flag_at(p.arg as usize);
    let lo = p.group_words + p.arg_words;
    match (flags, words > p.arg_words) {
        (true, _) => d.null_arg((lo + fw) as u8, bit as u8).group_flag_words(lo as u8, (acc.nkeys() - p.group_words) as u8),
        (false, true) => d.null_arg((pair.nkeys() - 1) as u8, p.arg),
        (false, false) => d,
    }
}

fn make<'g, 'p>(px: &mut Px<'g, 'p>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    let node = px.gx.pipelines()[k].node_id;
    let Some(g) = px.gx.group_at(k) else { shape(node, "a grouped DISTINCT end without its group") };
    // a tagged multi-argument form (decompose `multi_grouped`): one pair set over every argument, unless regroup.rs left it
    // host-sorted's to name (`pairs_multi::blocked`), which it then refuses as below
    let tagged = crate::ops::shared::agg::tagged_multi(g);
    let inner = if tagged && !crate::ops::group::pairs_multi::blocked(g) {
        Inner::Multi(crate::ops::group::pairs_multi::make(px, k, g, node))
    } else {
        // D536: a second DISTINCT argument has no record slot (decompose keeps one pair set): M6's keyed.distinct_multi ends
        // the group (M6 L0-e; NotBuilt by its name until registered), as keyed.distinct_plain hands it (V4R-183)
        if multi(g) || tagged {
            if g.aggs.iter().any(|d| matches!(d.final_, FinalForm::PairCount | FinalForm::PairFold) && d.filter.is_some()) {
                shape(node, "a FILTER on a DISTINCT aggregate beside a second DISTINCT argument");
            }
            crate::mechs_used::declined(crate::admit::refusal::Variant::Kind(KindId::KeyedDistinctGrouped)); // MECHCORPUS: the node is another kind's form
            return (crate::frame::iface::kind_impl::<EndMaker>(&node, KindId::KeyedDistinctMulti).0)(px, k);
        }
        if !crate::graph::order_free(g) { // D859: PG's GroupAggregate over Sort(keys, arg) feeds every aggregate in that order
            shape(node, "a rider whose value reads the row order beside a DISTINCT");
        }
        let mut pair = pair_of(px.gx, g, node);
        let inner = (crate::frame::iface::kind_impl::<EndMaker>(&node, KindId::KeyedHashagg).0)(px, k);
        pair.dense = dense_of(px, g);
        Inner::Hash(inner, pair)
    };
    mcx::pg_box_dyn!(px.mcx(), PairEnd { inner } => dyn EndMech + 'g)
}

/// keyed.hashagg's end over the pair record, its owner phase the distinct owner's; or the tagged form's end
/// (pairs_multi.rs), which declares its own owner.
enum Inner<'g, 'p> {
    Hash(PgBox<'g, dyn EndMech + 'g>, Pair<'g>),
    Multi(crate::ops::group::pairs_multi::MultiEnd<'g, 'p>),
}

struct PairEnd<'g, 'p> {
    inner: Inner<'g, 'p>,
}

impl<'g, 'p> PairEnd<'g, 'p> {
    fn end(&self) -> &dyn EndMech {
        match &self.inner {
            Inner::Hash(e, _) => &**e,
            Inner::Multi(e) => e,
        }
    }
}

impl EndMech for PairEnd<'_, '_> {
    fn state_bytes(&self) -> u32 {
        self.end().state_bytes()
    }
    fn open_words(&self, w: &mut OpenWords) {
        self.end().open_words(w)
    }
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        self.end().open(b, env)
    }
    fn carried<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>) -> PgVec<'e, Value> {
        self.end().carried(b, env)
    }
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, x: &Exits) -> Carry<'e> {
        self.end().lower(b, env, row, x)
    }
    fn window_end(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, c: &Carry) {
        self.end().window_end(b, env, c)
    }
    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) {
        self.end().finish(b, env, x)
    }
    fn sink(&self) -> SinkKind {
        self.end().sink()
    }
    fn whole(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, win: &Win, c: &Carry, l_wend: Block) -> bool {
        self.end().whole(b, env, win, c, l_wend)
    }
    fn block<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, w: &WordWin, base: Value, m: Mask, c: &Carry) -> Option<Carry<'e>> {
        self.end().block(b, env, w, base, m, c)
    }
    fn block_or_walk<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, w: &WordWin, base: Value, m: Mask, c: &Carry) -> Option<(Carry<'e>, Block)> {
        self.end().block_or_walk(b, env, w, base, m, c)
    }
    fn counts(&self, env: &Env) -> bool {
        self.end().counts(env)
    }
    fn takes_mult(&self) -> bool {
        false
    }
    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize, d: Pipeline) -> Pipeline {
        let Inner::Hash(inner, pair) = &self.inner else { return self.end().pipeline_desc(px, k, d) };
        let mut d = inner.pipeline_desc(px, k, d);
        let Some(o) = d.owner else { return d };
        // D563: the hash end declared the count form (runs/owner_count.rs, over the accumulator's slots) when a top applies
        let top = (o.topk > 0 && o.order_slot != ORDER_NONE && o.floor == 0).then_some(o);
        if top.is_some_and(|t| t.check != gravity_program::KeyCheck::NONE) {
            crate::refuse!(px.gx.pipelines()[k].node_id, Reason::Bug { payload: "a count(DISTINCT) count top with a key check (count_cut declares none; distinct() would drop it)" })
        }
        let o = owner(px.arena, o.layout, pair, top.as_ref(), px.mcx());
        d.owner = Some(o);
        if let Some((attno, base)) = pair.dense {
            d.owner = Some(o.dense_key(attno));
            let none = WordsDesc::NONE;
            d.words = Some(WordsDesc { topk: none, dense_lo: base, dense_hi: base + 1, bits: none, order: none, nparams: base + 2 });
        }
        d
    }
}

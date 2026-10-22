//! The tagged pair form of a grouped multi-argument count(DISTINCT) (keyed.distinct_grouped's maker hands it here;
//! decompose ops/agg/mod.rs `multi_grouped`: the group keys, then one resno-0 key per DISTINCT argument, each call a
//! `PairCount` naming its argument's key in `count_slot`).
//!
//! ONE table of pairs `[group key image, tag, argument image]`: the tag is the argument's index among the resno-0 keys, the
//! argument's image padded with 0 words to the widest argument's (a pair's tag decides its argument, so pads never meet
//! another argument's words), then the flags word where some key is nullable (the group keys' bits, and on a tag-0 pair the
//! argument's at bit `gk`: D500's group-flags form), then the riders' slots and the implicit count sentinel (so a pair that
//! folds nothing is still a live record). Each row upserts its tag-0 pair, a NULL argument flagged (its riders still fold,
//! no pair counts), and folds every rider there (keyed.hashagg's open and steps); each other argument that is not NULL
//! (count's strict input) upserts its pair, which folds nothing. The distinct owner (`OwnerDesc::distinct(..).tagged`,
//! exec owner.rs) dedups the pairs, keeps one accumulator per group (the group words, and the flags word with the
//! argument's bit cleared), counts each new pair into its tag's slot and merges only tag-0 pairs' riders; the emit reads
//! one row per accumulator (`view`). Every pipeline stays parallel: no sort, no hosted state.
use gravity_abi::breaker::{KeyKind, RecordLayout, SlotKind, ACC_PAIRS, ORDER_NONE};
use gravity_abi::hash::Depth;
use gravity_decompose::ops::shared::agg::{FinalForm, GroupDecl, NONE};
use gravity_expr::emit::hash::{emit_image, emit_key_hash, KeyPart};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type, Value};
use gravity_program::{OwnerDesc, Pipeline, WordsDesc, MAX_REPS, REP_NONE};

use crate::admit::refusal::Reason;
use crate::face::end::EndMech;
use crate::frame::env::Env;
use crate::frame::open::OpenWords;
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::ops::agg::hashagg::{AccAt, GroupRec, KeyVal};
use crate::table::face::{Domain, KeyShape, TableDecl, TableMech};
use crate::table::keyimg::ImageWords;

fn bug(node: i32, payload: &'static str) -> ! {
    crate::refuse!(node, Reason::Bug { payload })
}

/// Whether tagged group `g` is one this form does not serve (ops/group/regroup.rs re-declares it host-sorted, the form decompose
/// gave the node before): a rider with no record slot (a hosted state), a grouping set, a two-pass plan or hint, an
/// order-key layout (D3374's keyed extremes), a record past the owner's byte word ids. A FILTER on a DISTINCT call is its
/// key's (table/rec.rs `key_vals`: the argument NULL where it does not pass).
pub(crate) fn blocked(g: &GroupDecl<'_>) -> bool {
    let hosted = g.aggs.iter().any(|d| d.final_ != FinalForm::PairCount && d.slot == NONE);
    let wide = g.layout.keys.len() + gravity_abi::breaker::flag_words(g.keys.len()) > u8::MAX as usize;
    hosted || wide || g.set.is_some() || g.two_pass.is_some() || g.hints.iter().any(Option::is_some) || g.layout.ties
}

/// The tagged pair's shape over a group record `rec` (`GroupRec::new`'s, its layout the image of every key).
pub(crate) struct Tagged {
    pub pair: RecordLayout,
    /// The group keys, their image words, the arguments, the widest argument's words, and whether the flags word is kept.
    pub gk: usize,
    pub gw: usize,
    pub tags: usize,
    pub w: usize,
    pub flags: bool,
}

impl Tagged {
    pub(crate) fn of(arena: gravity_program::ArenaRef, rec: &GroupRec<'_, '_>) -> Tagged {
        let g = rec.g;
        let gk = g.keys.iter().position(|k| k.resno == 0).unwrap_or_else(|| bug(rec.node, "a tagged DISTINCT group without its argument keys"));
        let gw: usize = rec.wpk[..gk].iter().map(|&w| w as usize).sum();
        let w = rec.wpk[gk..].iter().map(|&w| w as usize).max().unwrap_or(1);
        let flags = rec.nullable;
        if gravity_abi::breaker::flag_words(g.keys.len()) > 1 {
            bug(rec.node, "a tagged DISTINCT group past one flags word (decompose multi_grouped keeps 64 keys)");
        }
        // the pair's key kinds on the stack: the group words', then I64 for the tag, the argument words and the flags word
        // (`blocked` keeps the group record within 255 words, so the pair within 256)
        let n = gw + 1 + w + flags as usize;
        let mut kinds = [KeyKind::I64; 256];
        if n > kinds.len() {
            bug(rec.node, "a tagged DISTINCT pair past 256 words");
        }
        kinds[..gw].copy_from_slice(&rec.layout.key_kinds()[..gw]);
        let l = &rec.layout;
        let implicit = l.flags & gravity_abi::breaker::LAYOUT_IMPLICIT_COUNT != 0;
        let slots = &l.slot_kinds()[..l.nslots() - implicit as usize];
        let pair = gravity_program::record_layout_at(arena, &kinds[..n], slots, l.has_validity(), None);
        Tagged { pair, gk, gw, tags: g.keys.len() - gk, w, flags }
    }

    /// The owner's accumulator: the group words (and the flags word), `[sentinel, pairs of tag 0]`, the riders, then one
    /// `Count` slot per further tag; with the first of those.
    pub(crate) fn acc(&self, arena: gravity_program::ArenaRef) -> (RecordLayout, u8) {
        let (base, _) = OwnerDesc::accumulator_folded(arena, &self.pair, (1 + self.w) as u8, &[], self.flags);
        // on the stack: a record's slots are at most 255 (`nslots` is a byte) and the tags at most 64 (one flags word, `of`)
        let (nb, n) = (base.nslots(), base.nslots() + self.tags - 1);
        let mut slots = [SlotKind::Count; 320];
        slots[..nb].copy_from_slice(base.slot_kinds());
        (gravity_program::record_layout(arena, base.key_kinds(), &slots[..n], base.has_validity()), nb as u8)
    }

    /// The accumulator slot tag `t`'s count is in.
    pub(crate) fn slot(&self, t: usize, first: u8) -> u16 {
        if t == 0 { u16::from(ACC_PAIRS) } else { u16::from(first) + t as u16 - 1 }
    }
}

/// The tagged end (pairs.rs `make` boxes it with its other ends).
pub(crate) fn make<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, g: &'g GroupDecl<'p>, node: i32) -> MultiEnd<'g, 'p> {
    let mut rec = GroupRec::new(px, g);
    if rec.handle.is_some() || rec.layout.partition_keys() < rec.layout.nkeys() {
        bug(node, "a tagged DISTINCT group with hosted states or a partitioned record");
    }
    rec.compile_args(px, false);
    let tagged = Tagged::of(px.arena, &rec);
    (rec.image, rec.layout) = (Some(rec.layout), tagged.pair);
    let mut t = TableDecl { image_words: tagged.pair.nkeys() as u16, record_bytes: tagged.pair.size() - 8 * tagged.pair.nkeys() as u32, expect: None, depth: Depth::TOP, life: rec.home, layout: tagged.pair, state_off: 0, key: KeyShape { code: None, dense: None, domain: Domain::Unknown }, facts: 0, copies: crate::table::emit_key::copy_slots(&rec) };
    t.facts = crate::table::size::declare_facts(px, k, &t, 0);
    // D2988: a table reopened per rescan opens at the planner's estimate
    if crate::rerun::sets::in_owner_set(px.gx, k) {
        t.expect = Some(g.groups).filter(|&n| n > 0);
    }
    let table = crate::table::face::table(&node, &t);
    let dense = crate::ops::group::pairs::dense_of(px, g);
    fires::fire("agg.distinct_multi_grouped"); // shapecheck fire signal: the tagged pair end
    MultiEnd { rec, t, table, tagged, dense }
}

pub(crate) struct MultiEnd<'g, 'p> {
    rec: GroupRec<'g, 'p>,
    t: TableDecl,
    table: &'static dyn TableMech,
    tagged: Tagged,
    /// V4R-194: the one group key's dense domain (pairs.rs `dense_of`).
    dense: Option<(i16, u16)>,
}

impl<'p> MultiEnd<'_, 'p> {
    /// Tag `t`'s pair over the image `img` of every key (`keys` its cells): the group words, the tag, the argument's words
    /// padded with 0 to the widest, the flags word with the group keys' bits (and the argument's on tag 0).
    fn pair(&self, b: &mut FunctionBuilder<'_>, env: &Env, img: &ImageWords, t: usize) -> ImageWords {
        let Tagged { gk, gw, w, flags, .. } = self.tagged;
        let (at, n) = self.rec.key_words(gk + t);
        let src = &img.words.words;
        let tag = [b.iconst(Type::I64, t as i64)];
        let pad = [env.zero; 256];
        let fw = flags.then(|| {
            let f = *src.last().unwrap_or_else(|| bug(self.rec.node, "a nullable image without its flags word"));
            let mask = b.iconst(Type::I64, ((1u64 << (gk + (t == 0) as usize)) - 1) as i64);
            b.int_bin(IntBinOp::And, f, mask)
        });
        // the parts in order (none nullable: emit_image only joins their words)
        fn part(words: &[Value]) -> KeyPart<'_> {
            KeyPart { words, null: None }
        }
        let parts = [part(&src[..gw]), part(&tag), part(&src[at..at + n]), part(&pad[..w - n]), part(fw.as_slice())];
        ImageWords::plain(emit_image(b, &parts), img.exact)
    }

    /// A fresh tag-0 record's rider slots initialised and its key representatives written (keyed.hashagg's `open_rec`).
    fn open_rec(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, rec: Value, fresh: Value, reps: &[Option<Value>]) {
        let (l_init, l_go) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(fresh, (l_init, &[]), (l_go, &[]));
        b.switch_to(l_init);
        for a in &self.rec.aggs {
            a.class.init(b, env, &self.rec.decl(a), self.rec.slot(rec, a.d.slot));
        }
        for (k, w) in self.rec.g.keys.iter().zip(reps).filter_map(|(k, w)| w.map(|w| (k.rep, w))) {
            b.store(rec, self.rec.layout.slot_off(k as usize) as i32, w);
            let set = {
                let none = b.const_raw(Type::I64, gravity_abi::breaker::MINCODE_NONE);
                b.icmp(IntCond::Ne, w, none)
            };
            crate::ops::shared::slots::mark(b, env, rec, crate::ops::shared::slots::slot_valid(&self.rec.layout, k), set);
        }
        b.br(l_go, &[]);
        b.switch_to(l_go);
    }

    /// The row's tag-0 pair: upserted, opened, every rider stepped into it.
    fn tag0(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, img: &ImageWords) {
        let pair = self.pair(b, env, img, 0);
        let h = emit_key_hash(b, &pair.words);
        env.rec_window = None;
        let (rec, fresh) = self.table.upsert(b, env, &self.t, &pair, h);
        let window = env.rec_window.take();
        let reps = crate::table::emit_group::rep_words(&self.rec, b, env, row);
        let reps = crate::table::emit_key::keep_datums(&self.rec, b, env, row, fresh, &self.t, reps);
        self.open_rec(b, env, rec, fresh, &reps);
        let images = self.rec.images(b, env, &self.t);
        let zeroed = core::mem::replace(&mut env.rec_zeroed, true);
        env.rec_window = window;
        for a in &self.rec.aggs {
            self.rec.step(b, env, row, a, AccAt::Slot(rec, a.d.slot, images));
        }
        (env.rec_zeroed, env.rec_window) = (zeroed, None);
    }

    /// Tag `t`'s pair upserted (nothing folds) unless the argument's cell `kv` is NULL.
    fn tag(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, img: &ImageWords, kv: &KeyVal, t: usize) {
        let l_next = b.create_block(&[]);
        if let Some(n) = kv.2 {
            let l_do = b.create_block(&[]);
            b.cond_br(n, (l_next, &[]), (l_do, &[]));
            b.switch_to(l_do);
        }
        env.aside(|env| {
            let pair = self.pair(b, env, img, t);
            let h = emit_key_hash(b, &pair.words);
            env.rec_window = None;
            self.table.upsert(b, env, &self.t, &pair, h);
            env.rec_window = None;
        });
        b.br(l_next, &[]);
        b.switch_to(l_next);
    }
}

impl EndMech for MultiEnd<'_, '_> {
    fn state_bytes(&self) -> u32 {
        self.table.state_bytes(&self.t)
    }

    fn open_words(&self, w: &mut OpenWords) {
        self.table.open_words(&self.t, w);
    }

    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        self.table.open(b, env, &self.t);
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        let one = b.iconst(Type::I64, 1);
        let out = b.int_bin(IntBinOp::Add, row.carry.out, one);
        let keys = self.rec.key_vals(b, env, row);
        let img = self.rec.image_of(b, env, &keys);
        self.tag0(b, env, row, &img);
        for t in 1..self.tagged.tags {
            self.tag(b, env, &img, &keys[self.tagged.gk + t], t);
        }
        Carry::new(out, env.mcx)
    }

    fn window_end(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _c: &Carry) {
        self.table.window_end(b, env, &self.t);
    }

    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) {
        self.table.finish(b, env, &self.t, x);
    }

    /// A DISTINCT pair record is not a copy count.
    fn takes_mult(&self) -> bool {
        false
    }

    /// The tagged distinct owner over the pair: the tag and the argument's words its DISTINCT words, the group-flags form
    /// where the flags word is kept (the tag-0 argument's NULL bit `gk`), each further tag counted into its own slot.
    fn pipeline_desc(&self, px: &Px<'_, '_>, _k: usize, d: Pipeline) -> Pipeline {
        let Tagged { pair, gk, gw, w, flags, .. } = self.tagged;
        let (acc, first) = self.tagged.acc(px.arena);
        let groups = self.rec.g.groups.min(u32::MAX as u64) as u32;
        let mut o = OwnerDesc::distinct(pair, acc, (pair.nkeys() - acc.nkeys()) as u8, groups, ORDER_NONE, 0, [(REP_NONE, 0); MAX_REPS]);
        if flags {
            let fw = (gw + 1 + w) as u8;
            o = o.null_arg(fw, gk as u8).group_flag_words(fw, 1);
        }
        let mut d = crate::table::emit_key::resident(&self.rec, Pipeline { words: None, ..d }).owner(o.tagged(first));
        if let Some((attno, base)) = self.dense {
            d.owner = d.owner.map(|o| o.dense_key(attno));
            let none = WordsDesc::NONE;
            d.words = Some(WordsDesc { topk: none, dense_lo: base, dense_hi: base + 1, bits: none, order: none, nparams: base + 2 });
        }
        d
    }
}

/// Tagged group `rec`'s record as its emit reads it (frame/pairs_view.rs `view` asks here, then moves the riders by
/// `ACC_SLOT_BASE` and points each count(DISTINCT) at its tag's slot, `Tagged::slot`): the owner's accumulator; the
/// arguments' words are gone. With the shape and the first further tag's slot.
pub(crate) fn view(rec: &mut GroupRec<'_, '_>, px: &Px<'_, '_>) -> (Tagged, u8) {
    let tagged = Tagged::of(px.arena, rec);
    let (acc, first) = tagged.acc(px.arena);
    (rec.layout, rec.nullable) = (acc, tagged.flags);
    for w in rec.wpk[tagged.gk..].iter_mut() {
        *w = 0;
    }
    (tagged, first)
}

#[cfg(test)]
mod tests {
    use gravity_abi::breaker::{KeyKind, SlotKind, ACC_PAIRS, ACC_SLOT_BASE};

    /// The pair `[group, tag, image, flags] -> [sum, sentinel]` keeps one accumulator per group: the group word and the flags
    /// word, `[sentinel, pairs]`, the rider, then the further tags' counts.
    #[test]
    fn the_tagged_pair_keeps_one_accumulator_per_group_with_a_count_per_tag() {
        let arena = gravity_program::Arena::new(1 << 16);
        let pair = gravity_program::record_layout_at(arena.handle(), &[KeyKind::I64; 4], &[SlotKind::SumI64], false, None);
        let t = super::Tagged { pair, gk: 1, gw: 1, tags: 3, w: 1, flags: true };
        let (acc, first) = t.acc(arena.handle());
        assert_eq!(acc.key_kinds(), &[KeyKind::I64, KeyKind::I64]);
        assert_eq!(acc.slot_kinds(), &[SlotKind::Count, SlotKind::Count, SlotKind::SumI64, SlotKind::Count, SlotKind::Count]);
        assert_eq!(first, ACC_SLOT_BASE + 1);
        assert_eq!((t.slot(0, first), t.slot(1, first), t.slot(2, first)), (u16::from(ACC_PAIRS), 3, 4));
        let d = gravity_program::OwnerDesc::distinct(pair, acc, (pair.nkeys() - acc.nkeys()) as u8, 16, gravity_abi::breaker::ORDER_NONE, 0, [(gravity_program::REP_NONE, 0); gravity_program::MAX_REPS]).null_arg(3, 1).group_flag_words(3, 1).tagged(first);
        assert_eq!((d.pair, d.tags), (2, first));
    }
}

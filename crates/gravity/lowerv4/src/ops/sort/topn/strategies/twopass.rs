//! Two-pass form, kind `keyed.twopass` (R4-L14, R4-L17; a count top whose admitted set is the dup set + the smallest singletons, then an
//! exact second pass): pass 1 counts the rows per key, the runtime lays the ADMITTED keys as an exact set (ProtoKind
//! TwoPass, HOSTS' `AdmitHdr` behind a protocol word), and pass 2 folds only the groups whose key it admits, probing the set
//! once per row or per run before the table.
//!
//! Two export rules, read by `kept_keys` over pass 1's finished owner region (its records' shape packed into the
//! protocol's `arg`, a bound word appended by HOSTS):
//! - top (the planner's TwoPass hint): the groups ordered by `count(*)` DESC under a LIMIT n, the pass-1 key a group key.
//!   A group counts at most its key's rows, so every key seen twice is admitted, and of the keys seen once the n first in
//!   the top's tie order on the key (`floor_desc` / `floor_nulls_first`): each of those is one group of count 1, so no
//!   count-1 group of an unadmitted key can outrank n admitted ones. The top must order by `count(*)` DESC, then (if at
//!   all) by the pass-1 key in the hint's direction; the n is a constant (a `Bound::Const` is in the program's key).
//! - floor (twopass.having): the keys counting over / at least K, K the HAVING literal's bound word.
//!
//! Budget 450 lines (files.tsv, +25 % gate). Owner M1-KEYED.
use gravity_abi::breaker::{Breaker, KeyKind, OwnerFns, SlotKind, Spill};
use gravity_abi::status::{unsupported, UNSUPPORTED_SHAPE};
use gravity_decompose::ops::shared::agg::{FinalForm, GroupDecl, GroupForm};
use gravity_decompose::ops::shared::agg_class::{Class, Trans};
use gravity_expr::emit::hash::{emit_home, emit_home_at, emit_home_shift, emit_key_hash, Bits, ImageValues};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, IntUnOp, Type, Value};
use gravity_kernels::table::{ADMIT_ALL, ADMIT_EMPTY, ADMIT_EMPTY_KEY, ADMIT_FILTER, ADMIT_FILTER_AT, ADMIT_FILTER_LEN, ADMIT_MASK, ADMIT_SET};
use gravity_kernels::host_abi::{sk_key, sk_null, SK_DESC, SK_NULLS_FIRST, SK_SLOT};
use gravity_program::protos::{Proto, ProtoKind};
use gravity_kernels::ops::sort::topn::twopass_top::TopExportSpec;
use gravity_program::{ArenaRef, LifeId, OwnerDesc, Pipeline};
use mcx::{GrowIn, PgBox};

use crate::face::end::EndMech;
use crate::table::keyimg::ImageWords;
use crate::face::kinds::{Face, KindId};
use crate::frame::env::Env;
use crate::frame::frame::EndMaker;
use crate::frame::iface::{breaker_of, breaker_of_mut};
use crate::frame::px::{ExtraPass, Px};
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::ops::shared::twopass::{Admit, AdmitLive, Rule, Split};

use crate::table::rec::GroupRec;

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { kinds: &[(KindId::KeyedTwopass, &EndMaker(make_pass1))], ..crate::ops::NONE };

fn bug(node: i32, payload: &'static str) -> ! {
    crate::refuse!(node, Reason::Bug { payload })
}

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

/// Pass 1's end: the hash aggregate's over the pass-1 group (its keys, the row count as its sentinel slot), keyed by part
/// too when it marks granules for its pass 2.
fn make_pass1<'g, 'p>(px: &mut Px<'g, 'p>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    if let Some(n) = crate::ops::sort::topn::strategies::twopass_bare::bare_n(px, k) {
        return crate::ops::sort::topn::strategies::twopass_bare::make(px, k, n); // V4R-197: the ladder's bare pass 1
    }
    let gx = px.gx;
    match gx.group_at(k).and_then(|g| pass1_rec(px, k, g)) {
        Some(rec) => {
            let split = top_rule(px, k, &rec).map_or(Split::One(None), Split::Top);
            crate::frame::iface::group_end_of(px, k, rec, None, Some(split), None)
        }
        None => crate::frame::iface::group_end(px, k),
    }
}

/// V4R-192: the top rule of the planner pass 1 `k` (its pass 2's hint, the drain's constant bound), whose owner exports
/// the admitted records alone (kernels twopass_top, the ladder's EXPORT_DUPS); `None` leaves every record to `admitted`.
fn top_rule(px: &Px<'_, '_>, k: usize, rec: &GroupRec<'_, '_>) -> Option<Rule> {
    let gx = px.gx;
    let (g, tp) = (0..gx.pipelines().len()).find_map(|q| gx.group_at(q).and_then(|h| h.two_pass.filter(|t| t.pass1 as usize == k).map(|t| (h, t))))?;
    let q = gx.drainers(g).into_iter().find(|&q| gx.end_need(q) == KindId::RunsTopkGroups)?;
    let n = crate::ops::sort::topn::lower::bound(px, q).filter(|&n| (0..=0x7f_ffff).contains(&n))? as u32;
    // an extreme top's hint carries the extreme's order in its floor flags (extreme_pass2 checks them on the top's first word)
    let ext = extreme_of(rec.g).is_some();
    let (ext_nn, spread) = ext_slots(rec.g);
    let rule = Rule { ext, ext_nn: ext && ext_nn, spread: ext && spread, desc: tp.floor_desc, nulls_first: tp.floor_nulls_first, n, ..Rule::of(rec, rec.layout.sentinel_off(), rec.node) };
    (rule.img_words <= 2 && rec.layout.sentinel_slot() < rec.layout.nslots()).then_some(rule)
}

/// The export owner of a top rule (`Split::Top`'s `OwnerDesc.topk`: n, then the tie order, the image's words, an extreme
/// top's two bits).
fn top_owner(d: &OwnerDesc, _words: &[u64]) -> OwnerFns {
    let t = d.topk;
    let spec = TopExportSpec { layout: d.layout, count_slot: d.layout.sentinel_slot() as u8, img_words: (t >> 26 & 3) as u8, n: t & 0x7f_ffff, desc: t >> 23 & 1 != 0, nulls_first: t >> 24 & 1 != 0, nullable: t >> 25 & 1 != 0, ext: t >> 28 & 1 != 0, ext_nn: t >> 29 & 1 != 0, spread: t >> 30 & 1 != 0 };
    gravity_kernels::ops::sort::topn::twopass_top::owner_fns(&spec)
}

/// The planner pass 1 `k`'s record (group `g`) when a pass 2's hint names `k`: the count alone; `None`: decompose's
/// record (an image past two words, or no pass 2 names it).
fn pass1_rec<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, g: &'g GroupDecl<'p>) -> Option<GroupRec<'g, 'p>> {
    let gx = px.gx;
    (0..gx.pipelines().len()).find(|&q| gx.group_at(q).and_then(|h| h.two_pass).is_some_and(|t| t.pass1 as usize == k))?;
    if extreme_of(g).is_some() {
        // decompose's record `[image][count(*)][extreme][count(v)]` as declared, unmarked (the extreme rule reads one record a key)
        let mut r = GroupRec::new(px, g);
        if r.layout.nkeys() > 2 + fp_flags(&r) as usize {
            return None;
        }
        r.layout = r.layout.partitioned_by(r.layout.nkeys() as u8);
        r.home = g.producer;
        return Some(r);
    }
    // V4R-192: a key's records route by its image, so one owner sums them (kernels twopass_top)
    GroupRec::counts(px, g).map(|mut r| {
        r.layout = r.layout.partitioned_by(r.layout.nkeys() as u8);
        r
    })
}

impl Rule {
    /// Bits 1, 2 and 63 are the floor's `gt` / `int4` / `below`, a top-count rule's `lead` (its bits 0, 1, 2), else the
    /// extreme top's `ext` / `ext_nn` / `spread` (a rule is one of the three).
    pub fn pack(&self, word: Option<u16>) -> u64 {
        let lead = |i: u32| self.lead >> i & 1 != 0;
        let (b1, b2, b63) = match (self.floor, self.top_count) {
            (true, _) => (self.gt, self.int4, self.below),
            (false, true) => (lead(0), lead(1), lead(2)),
            (false, false) => (self.ext, self.ext_nn, self.spread),
        };
        let bits = [self.floor, b1, b2, self.desc, self.nulls_first, self.nullable];
        let flags = bits.iter().enumerate().fold(0u64, |a, (i, &b)| a | (b as u64) << i);
        let w = word.map_or(0, |w| w as u64 + 1);
        debug_assert!(w < 1 << 15, "a bound word's ordinal fits bits 48..62");
        debug_assert!(self.lead < 8 && (self.lead == 0 || self.top_count), "a lead is a top-count rule's, three bits");
        let n = (self.n as u64 & 0x7f_ffff) << 24;
        flags | (self.img_words as u64) << 6 | (self.rec_words as u64) << 8 | (self.count_word as u64) << 16 | n | (self.top_count as u64) << 47 | (w & 0x7fff) << 48 | (b63 as u64) << 63
    }

    pub fn unpack(a: u64) -> Rule {
        let f = |i: u32| a >> i & 1 != 0;
        let floor = f(0);
        let top_count = !floor && f(47);
        let ext = !floor && !top_count;
        Rule {
            floor,
            gt: floor && f(1),
            int4: floor && f(2),
            below: floor && f(63),
            ext: ext && f(1),
            ext_nn: ext && f(2),
            spread: ext && f(63),
            desc: f(3),
            nulls_first: f(4),
            nullable: f(5),
            img_words: (a >> 6 & 3) as u8,
            rec_words: (a >> 8) as u8,
            count_word: (a >> 16) as u8,
            n: (a >> 24 & 0x7f_ffff) as u32,
            top_count,
            lead: if top_count { f(1) as u8 | (f(2) as u8) << 1 | (f(63) as u8) << 2 } else { 0 },
        }
    }

    /// The shape of pass 1's records `rec` (the whole image exported), its count at byte `count_off`.
    pub fn of(rec: &GroupRec<'_, '_>, count_off: u32, node: i32) -> Rule {
        let (n, fp) = (rec.layout.nkeys(), fp_flags(rec));
        if n > 2 + fp as usize {
            shape(node, "a two-pass key image wider than two words");
        }
        let size = rec.layout.size();
        if size % 8 != 0 || size / 8 > 255 || count_off % 8 != 0 {
            shape(node, "a pass-1 record past 255 words");
        }
        Rule { img_words: n as u8, nullable: rec.nullable && !fp, rec_words: (size / 8) as u8, count_word: (count_off / 8) as u8, ..Rule::default() }
    }
}

/// A lone nullable Fp128 key (a derived text key, e.g. `regexp_replace(t, ..)`): its image is `(fp0, fp1, flags)`, three words, and the admitted
/// set exports and probes the fingerprint's two (`key_of`; pass 2's probe reads `(w0, w1)`), the flags word left out.
pub(crate) fn fp_flags(rec: &GroupRec<'_, '_>) -> bool {
    rec.nullable && rec.wpk.as_slice() == [2] && rec.g.keys.first().is_some_and(|k| k.kind == KeyKind::Fp128)
}

impl Admit {
    /// At pass 2's open: a header word the protocol never wrote (0) exits UNSUPPORTED_SHAPE, an ERROR, where the first probe
    /// would fault on it and take the postmaster's sessions down with the backend.
    /// D3148: whether the set behind the word is the every-key set (`ADMIT_ALL`), read once a window
    /// (`EndMech::every_guard`): a window under it folds every row without the test.
    pub fn every(&self, b: &mut FunctionBuilder<'_>, env: &Env) -> Value {
        let hdr = b.load(Type::Ptr, env.params, 8 * self.word as i32);
        let (ekw, all) = (b.load(Type::I64, hdr, ADMIT_EMPTY_KEY), b.iconst(Type::I64, ADMIT_ALL as i64));
        b.icmp(IntCond::Eq, ekw, all)
    }

    pub fn check_open(&self, b: &mut FunctionBuilder<'_>, env: &Env) {
        let (hdr, l_ok) = (b.load(Type::I64, env.params, 8 * self.word as i32), b.create_block(&[]));
        let (lost, unsup) = (b.icmp(IntCond::Eq, hdr, env.zero), b.iconst(Type::I64, unsupported(UNSUPPORTED_SHAPE)));
        b.cond_br(lost, (env.x.l_exit, &[unsup]), (l_ok, &[]));
        b.switch_to(l_ok);
    }

    /// D3326: the set's header words for the rows that follow (`AdmitLive`), loaded where a row loop starts.
    pub fn live(&self, b: &mut FunctionBuilder<'_>, env: &Env) -> AdmitLive {
        let hdr = b.load(Type::Ptr, env.params, 8 * self.word as i32);
        let filter = b.load(Type::Ptr, hdr, ADMIT_FILTER);
        let fbits = b.load(Type::I64, hdr, ADMIT_FILTER_LEN);
        AdmitLive { hdr, filter, shift: emit_home_shift(b, fbits) }
    }

    /// Whether the group of `img` is admitted: pass 1's image of it passes the prefilter (V4R-209: both its bits set, so
    /// an empty set's clear filter admits nothing) and is found in the exact set (home = KeyHash of those words, probed
    /// linearly to a hole: the set is at most half full, so the probe needs no cancel point). `live`: the header words a
    /// row loop's start read (D3326), else this probe loads them.
    pub fn probe(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, rec: &GroupRec<'_, '_>, img: &ImageWords, live: Option<AdmitLive>) -> Value {
        let (w0, w1) = match self.key {
            None => (img.words.words[0], img.words.words.get(1).copied()),
            Some(key) => {
                let (first, n) = rec.key_words(key);
                // CAP-28: the key's NULL bit in its flags word (one word per 64 keys)
                let w1 = match (n, rec.null_flag(key).filter(|_| self.null_word)) {
                    (2, _) => Some(img.words.words[first + 1]),
                    (_, Some((fw, at))) => {
                        let s = b.iconst(Type::I64, at as i64);
                        let bit = b.int_bin(IntBinOp::Ushr, img.words.words[fw], s);
                        let one = b.iconst(Type::I64, 1);
                        Some(b.int_bin(IntBinOp::And, bit, one))
                    }
                    _ => None,
                };
                (img.words.words[first], w1)
            }
        };
        let h = emit_key_hash(b, &ImageValues { words: [Some(w0), w1].into_iter().flatten().collect() }); // alloc: Program
        let w1 = w1.unwrap_or(env.zero);
        let (hdr, filter, shift, fbits) = match live {
            Some(l) => (l.hdr, l.filter, Some(l.shift), env.zero),
            None => {
                let hdr = b.load(Type::Ptr, env.params, 8 * self.word as i32);
                (hdr, b.load(Type::Ptr, hdr, ADMIT_FILTER), None, b.load(Type::I64, hdr, ADMIT_FILTER_LEN))
            }
        };
        let mut both = b.iconst(Type::I64, 1); // bit 0 of each probed word, shifted down
        for at in ADMIT_FILTER_AT {
            let p = match shift {
                Some(s) => emit_home_at(b, h, s, Bits::Const(at.used())),
                None => emit_home(b, h, Bits::Dyn(fbits), Bits::Const(at.used())),
            };
            let s6 = b.iconst(Type::I64, 6);
            let wi = b.int_bin(IntBinOp::Ushr, p, s6);
            let fw = b.ptr_add(filter, wi, 8, 0);
            let fw = b.load(Type::I64, fw, 0);
            let bit = b.int_bin(IntBinOp::Ushr, fw, p);
            both = b.int_bin(IntBinOp::And, both, bit);
        }
        let l_out = b.create_block(&[Type::Bool]);
        let l_look = b.create_block(&[]);
        let pass = b.icmp(IntCond::Ne, both, env.zero);
        let f = b.bconst(false);
        b.cond_br(pass, (l_look, &[]), (l_out, &[f]));
        b.switch_to(l_look);
        let mask = b.load(Type::I64, hdr, ADMIT_MASK);
        let set = b.load(Type::Ptr, hdr, ADMIT_SET);
        let bits = b.int_un(IntUnOp::Popcnt, mask);
        let home = emit_home(b, h, Bits::Dyn(bits), Bits::Const(0));
        let head = b.create_block(&[Type::I64]);
        b.br(head, &[home]);
        b.switch_to(head);
        b.mark_loop_header(head);
        env.bounded.push_in(head);
        let i = b.block_params(head)[0];
        let at = b.ptr_add(set, i, 16, 0);
        let (a, c) = (b.load(Type::I64, at, 0), b.load(Type::I64, at, 8));
        let (e0, e1) = (b.icmp(IntCond::Eq, a, w0), b.icmp(IntCond::Eq, c, w1));
        let hit = b.int_bin(IntBinOp::And, e0, e1);
        let t = b.bconst(true);
        let l_miss = b.create_block(&[]);
        b.cond_br(hit, (l_out, &[t]), (l_miss, &[]));
        b.switch_to(l_miss);
        let ev = b.iconst(Type::I64, ADMIT_EMPTY as i64);
        let (x0, x1) = (b.icmp(IntCond::Eq, a, ev), b.icmp(IntCond::Eq, c, ev));
        let hole = b.int_bin(IntBinOp::And, x0, x1);
        let l_next = b.create_block(&[]);
        let l_hole = b.create_block(&[]);
        b.cond_br(hole, (l_hole, &[]), (l_next, &[]));
        b.switch_to(l_hole);
        // the empty pair itself is a real key only through `empty_key`; `ADMIT_ALL` admits every key (D459: a spilled pass 1 with no owner result)
        let (k0, k1) = (b.icmp(IntCond::Eq, w0, ev), b.icmp(IntCond::Eq, w1, ev));
        let is_empty = b.int_bin(IntBinOp::And, k0, k1);
        let ekw = b.load(Type::I64, hdr, ADMIT_EMPTY_KEY);
        let ek = b.icmp(IntCond::Ne, ekw, env.zero);
        let r = b.int_bin(IntBinOp::And, is_empty, ek);
        let all = b.iconst(Type::I64, ADMIT_ALL as i64);
        let all = b.icmp(IntCond::Eq, ekw, all);
        let r = b.int_bin(IntBinOp::Or, r, all);
        b.br(l_out, &[r]);
        b.switch_to(l_next);
        let one = b.iconst(Type::I64, 1);
        let i2 = b.int_bin(IntBinOp::Add, i, one);
        let i2 = b.int_bin(IntBinOp::And, i2, mask);
        b.br(head, &[i2]);
        b.switch_to(l_out);
        b.block_params(l_out)[0]
    }
}

/// Under a two-pass form (`admit`), the fold that follows runs only for an admitted group: the block after it (`join`).
pub(crate) fn admitted_at(admit: Option<Admit>, b: &mut FunctionBuilder<'_>, env: &mut Env, rec: &GroupRec<'_, '_>, img: &ImageWords, live: Option<AdmitLive>) -> Option<gravity_ir::Block> {
    let ok = admit?.probe(b, env, rec, img, live);
    let (l_do, l_join) = (b.create_block(&[]), b.create_block(&[]));
    b.cond_br(ok, (l_do, &[]), (l_join, &[]));
    b.switch_to(l_do);
    Some(l_join)
}

pub(crate) fn join(b: &mut FunctionBuilder<'_>, l: Option<gravity_ir::Block>) {
    if let Some(l) = l {
        b.br(l, &[]);
        b.switch_to(l);
    }
}

/// The admitted set's word and protocol for pass 2 `pass2` of pass 1 `pass1` (slot `slot`): one protocol word, the rule's
/// shape and bound word in `arg`.
pub(crate) fn declare(px: &mut Px<'_, '_>, pass2: usize, pass1: usize, slot: u8, rule: &Rule, bound: Option<u16>) -> u16 {
    let word = px.proto_words(1);
    let p = Proto::new(ProtoKind::TwoPass, pass2 as u16).peer(pass1 as u16).slot(slot).words(word, 1).arg(rule.pack(bound)).reading(0, crate::ops::sort::topn::raw::kept_keys);
    px.declare_proto(p);
    word
}

/// The pipeline draining group `g` into a top over its groups that pass 2's admission may serve: one keeping the groups'
/// records by reference (`runs.topk_groups`), or one keeping its rows by value only because a later breaker produces the
/// group's slot or its own again before the top's reader (`Gx::top_reused`, `runs.topk_rows`: a plain aggregate over the
/// top, whose breaker takes the group's slot again; e2e group_twopass_count_top). The top's
/// order and bound are the same Sort either way, so the same groups reach it. Never a top over a group another pipeline
/// drains too, or one in an owner's rerun set (`runs.topk_rows` there as well): those read every group, which the
/// admission would leave out.
pub(crate) fn top_drain(gx: &crate::graph::Gx<'_, '_>, g: &GroupDecl<'_>) -> Option<usize> {
    (0..gx.pipelines().len()).find(|&q| {
        gx.drained_group(q).is_some_and(|d| core::ptr::eq(d, g))
            && match gx.end_need(q) {
                KindId::RunsTopkGroups => true,
                KindId::RunsTopkRows => gx.top_reused(q) && !gx.drained_twice(q) && !crate::rerun::sets::in_owner_set(gx, q),
                _ => false,
            }
    })
}

/// The extreme a planner pass 1 folds (decompose `pass1` under an extreme top: `max` / `min` of the order's column), if any.
pub(crate) fn extreme_of<'p>(g: &GroupDecl<'p>) -> Option<Class> {
    g.aggs.iter().map(|d| d.class).find(|c| matches!(c, Class::Min(_) | Class::Max(_)))
}

/// An extreme pass 1's optional words (decompose `pass1`: `[count(*)][witness][count(v)][best]`): whether it counts its
/// column's values (`ext_nn`) and whether it folds a best apart from its witness (`spread`, gtopn GT-1).
pub(crate) fn ext_slots(g: &GroupDecl<'_>) -> (bool, bool) {
    let nn = g.aggs.iter().any(|d| d.class == Class::CountAny);
    (nn, g.aggs.iter().filter(|d| matches!(d.class, Class::Min(_) | Class::Max(_))).count() == 2)
}

/// Pass 2 of an extreme top (pass 1 `g1` folds the extreme `e1` per key): the top orders the groups first by the group's
/// own extreme of `e1`'s kind, in the order whose best is the extreme (`max` DESC, `min` ASC), either NULL placement; the
/// rule admits every key whose best possible group ranks with the n-th best key extreme (raw.rs `ext_admitted`).
#[allow(clippy::too_many_arguments)]
fn extreme_pass2<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, g: &GroupDecl<'_>, rec: &GroupRec<'_, '_>, sorts: &[u64], g1: &'g GroupDecl<'p>, e1: Class, n: u32) -> Admit {
    let (node, tp) = (g.node_id, g.two_pass.expect("an extreme pass 2 has a pass 1"));
    // gtopn GT-1: the top's first word orders an aggregate whose bounds are pass 1's (`Class::two_pass_bounds`: its
    // witness extreme `e1`, its best when spread), in the hint's direction
    let desc = tp.floor_desc;
    let (ext_nn, spread) = ext_slots(g1);
    let bounds = (matches!(e1, Class::Max(_)), spread.then_some(desc));
    let first = sorts.first().copied().unwrap_or(0);
    let want = SK_SLOT | if desc { SK_DESC } else { 0 };
    if !rec.aggs.iter().any(|a| a.d.class.two_pass_bounds(desc) == Some(bounds) && a.d.filter.is_none() && first & !SK_NULLS_FIRST == want | a.d.slot as u64) {
        shape(node, "a two-pass extreme top not ordered by its extreme first");
    }
    let p1 = px.gx.pipelines()[tp.pass1 as usize].node_id;
    let Some(rec1) = pass1_rec(px, tp.pass1 as usize, g1) else { bug(p1, "an extreme pass 1 whose key image is past two words") };
    if g1.keys.len() != 1 {
        bug(p1, "a pass 1 keyed by more than the hint's one key");
    }
    // `[image][count(*)][witness][count(v)][best]`, a word each (decompose pass1's slots, the sentinel first)
    let (l, cw) = (&rec1.layout, rec1.layout.sentinel_off());
    let nwords = 1 + ext_nn as usize + spread as usize;
    if l.sentinel_slot() != 0 || (1..=nwords).any(|j| l.slot_off(j) != cw + 8 * j as u32) || rec1.aggs.len() != nwords {
        bug(p1, "an extreme pass-1 record not laid out count, witness, count of values, best");
    }
    let rule = Rule { ext: true, ext_nn, spread, desc, nulls_first: first & SK_NULLS_FIRST != 0, n, ..Rule::of(&rec1, cw, node) };
    if (tp.floor_desc, tp.floor_nulls_first) != (rule.desc, rule.nulls_first) {
        bug(node, "an extreme top whose hint's order is not its first sort word's"); // pass 1's export ranks by the hint's
    }
    let word = declare(px, px.ident(k), tp.pass1 as usize, tp.slot, &rule, None);
    fires::fire("keyed.twopass.extreme"); // shapecheck fire signal (REGISTRY KEYED-03): an extreme top's admission
    Admit { word, key: Some(tp.key as usize), null_word: rule.nullable, every_window: false }
}

/// Pass 2 of the planner's TwoPass (group `g` at pipeline `k`, its pass 1 the hint's): the top's shape checked, the rule
/// declared, the admission for the group's end; `None` when `g` has no pass 1.
pub(crate) fn planner_pass2(px: &mut Px<'_, '_>, k: usize, g: &GroupDecl<'_>, rec: &GroupRec<'_, '_>) -> Option<Admit> {
    let tp = g.two_pass?;
    let node = g.node_id;
    let gx = px.gx;
    let key = tp.key as usize;
    let Some(q) = top_drain(gx, g) else { shape(node, "a two-pass group whose drain is not a top over its groups") };
    let extreme = gx.group_at(tp.pass1 as usize).and_then(|g1| extreme_of(g1).map(|e| (g1, e)));
    // only the lead this admission checks (an extreme top's extreme; a count top's count, then the pass-1 key's tie word):
    // it runs inside the group's producer, before its site is numbered, and a key past the lead is the emit's to image
    // (`sort_words_upto`: a text code key's word names that site)
    let (sorts, _) = crate::ops::sort::topn::lower::sort_words_upto(px, q, rec, if extreme.is_some() { 1 } else { 2 });
    if let Some((g1, e1)) = extreme {
        let n = match crate::ops::sort::topn::lower::bound(px, q) {
            Some(n) if n <= 0x7f_ffff => n as u32,
            _ => shape(node, "a two-pass top over no constant bound under 2^23"),
        };
        return Some(extreme_pass2(px, k, g, rec, &sorts, g1, e1, n));
    }
    let count = rec.aggs.iter().find(|a| a.d.class == Class::CountStar && a.d.filter.is_none());
    let Some(count) = count else { shape(node, "a two-pass group without its count(*)") };
    if sorts.first() != Some(&(SK_SLOT | SK_DESC | count.d.slot as u64)) && sorts.first() != Some(&(SK_SLOT | SK_DESC | SK_NULLS_FIRST | count.d.slot as u64)) {
        shape(node, "a two-pass top not ordered by count(*) DESC first");
    }
    // the tie order compares the key word signed (kernels twopass_top): an int, date or timestamp word orders as its type
    let int_key = matches!(g.keys[key].kind, KeyKind::I64 | KeyKind::I32 | KeyKind::Date | KeyKind::Timestamp);
    if let Some(&w) = sorts.get(1) {
        let (word, _) = rec.key_words(key);
        let dir = (if tp.floor_desc { SK_DESC } else { 0 }) | if tp.floor_nulls_first { SK_NULLS_FIRST } else { 0 };
        // D596: a nullable key's sort word names its NULL flag's bit (D456, kernels `sk_null`); the rule's tie order ranks NULL
        // first or last as `floor_nulls_first` says (`Single`), the order the word's own NULLS FIRST / LAST gives
        let null_bit = rec.flags_word().map_or(0, |_| sk_null(key + 1));
        if w != sk_key(word) | null_bit | dir || !int_key {
            shape(node, "a two-pass top whose tie order is not the pass-1 key's");
        }
    }
    let n = match crate::ops::sort::topn::lower::bound(px, q) {
        Some(n) if n <= 0x7f_ffff => n as u32,
        _ => shape(node, "a two-pass top over no constant bound under 2^23"),
    };
    if crate::ops::sort::topn::strategies::twopass_bare::bare_n(px, tp.pass1 as usize).is_some() {
        // a nullable key probes its NULL flag as the image's second word (Admit::probe); the reader admits the NULL key
        let (word, nullable) = (px.proto_words(1), rec.flags_word().is_some());
        let rule = Rule { n, nullable, img_words: 1 + nullable as u8, ..Rule::default() };
        let p = Proto::new(ProtoKind::TwoPass, px.ident(k) as u16).peer(tp.pass1 as u16).slot(tp.slot).words(word, 1).arg(rule.pack(None)).reading(0, crate::ops::sort::topn::raw::bare_keys);
        px.declare_proto(p);
        return Some(Admit { word, key: Some(key), null_word: nullable, every_window: false }); // V4R-197: the bare pass 1's list
    }
    let p1 = gx.pipelines()[tp.pass1 as usize].node_id;
    let Some(g1) = gx.group_at(tp.pass1 as usize) else { bug(p1, "a two-pass group whose pass 1 has no group") };
    let rec1 = pass1_rec(px, tp.pass1 as usize, g1).unwrap_or_else(|| GroupRec::new(px, g1));
    if g1.keys.len() != 1 {
        bug(p1, "a pass 1 keyed by more than the hint's one key");
    }
    let rule = Rule { desc: tp.floor_desc, nulls_first: tp.floor_nulls_first, n, ..Rule::of(&rec1, rec1.layout.sentinel_off(), node) };
    let word = declare(px, px.ident(k), tp.pass1 as usize, tp.slot, &rule, None);
    Some(Admit { word, key: Some(key), null_word: rule.nullable, every_window: false })
}

/// Face::Keyed's make-time state: each group twopass.having split, `(slot, graph producer, pass 2's life)`. Pass 2 holds
/// the group's records, so the group's readers (emit, top, retained representatives) name its life.
#[derive(Default)]
pub(crate) struct KeyedExt {
    homes: Vec<(u8, u16, LifeId)>,
}
crate::frame::px::ext_g!(KeyedExt);

impl KeyedExt {
    pub fn home(&self, g: &GroupDecl<'_>) -> Option<LifeId> {
        self.homes.iter().find(|h| h.0 == g.slot && h.1 == g.producer.0).map(|h| h.2)
    }
}

impl Split {
    pub fn desc(self, arena: ArenaRef, node: i32, mut d: Pipeline) -> Pipeline {
        match (self, breaker_of(&d)) {
            (Split::Top(r), Some(_)) => {
                let t = r.n | (r.desc as u32) << 23 | (r.nulls_first as u32) << 24 | (r.nullable as u32) << 25 | (r.img_words as u32 & 3) << 26 | (r.ext as u32) << 28 | (r.ext_nn as u32) << 29 | (r.spread as u32) << 30;
                let owner = d.owner.map(|o| OwnerDesc { ctor_desc: Some(top_owner), topk: t, ..o });
                Split::One(None).desc(arena, node, Pipeline { owner, ..d })
            }
            (Split::One(s1), Some(b)) => {
                if let Some(e) = breaker_of_mut(&mut d) {
                    *e = Breaker { slot: s1.unwrap_or(b.slot), spill: Spill::None, ..b };
                }
                crate::frame::iface::end_holds(d, gravity_program::spill::Shape::Grows(gravity_program::spill::NoSpill::TwoPassPass1))
            }
            (Split::One(_) | Split::Top(_), None) => bug(node, "a split pass 1 that ends in no breaker"),
            (Split::Two(s1, _), _) if d.consumes_from.is_empty() => d.consuming_in(arena, s1),
            (Split::Two(s1, from), _) => d.consuming_from_in(arena, s1, from),
        }
    }
}

/// Descriptor `d` reading group slot `slot` from `home` where it names the graph producer's life (a split group).
pub(crate) fn rehome(arena: ArenaRef, d: Pipeline, slot: u8, producer: LifeId, home: LifeId) -> Pipeline {
    if home == producer || d.consumes_from.is_empty() {
        return d;
    }
    let mut n = Pipeline { consumes: &[], consumes_from: &[], ..d };
    for (&s, &l) in d.consumes.iter().zip(d.consumes_from) {
        n = n.consuming_from_in(arena, s, if s == slot && l == producer { home } else { l });
    }
    n
}

/// twopass.having's split of graph pipeline `k`'s group `g` (`rec` its record): pass 1 (`k`) counts each key into a new
/// slot; pass 2, `k`'s scan remade right after it, folds every aggregate of the groups whose count passes the HAVING
/// floor (the TwoPass floor rule over pass 1's region) into the group's own slot, which the emit reads unchanged and
/// filters again. Taken when the folds weigh more than the second read and the admission probe (`fold_eighths`: a floor
/// that drops enough rows can pay for pass 2), over the rows reaching the group against every row the scan reads where
/// both are estimated (`second_read_pays`: a selective scan filter leaves the folds too little to spare), and pass 1's
/// image is at most two words; `None`: one pass, the floor applied at the emit. KEYED-02 / EX-15: which form runs is
/// chosen per execution (exec ops/sort/topn/proto.rs `having_cost`), from the mean group size and the fold weight the
/// floor rule carries (`having_estimate`) and the floor K this execution binds. Both passes read the same snapshot, and
/// M1 runs no VOLATILE call (expr.volatile is M2), so pass 2 counts what pass 1 counted. D-06b: a count top the owner
/// cuts past its HAVING (count_cut `suffix_cut`, D3152) takes the split too: its owner cuts pass 2's records
/// (keyed/hashagg.rs `end_of`), the admitted groups, or every group where this execution runs one pass; pass 2 counts
/// each group whole, so the cut ranks the same counts as in one pass.
pub(crate) fn having_split<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, g: &'g GroupDecl<'p>, rec: &GroupRec<'g, 'p>) -> Option<PgBox<'g, dyn EndMech + 'g>> {
    let gx = px.gx;
    let eighths: u64 = rec.aggs.iter().map(fold_eighths).sum();
    // D1339 row C: a nest's / SubPlan's rerun set member re-runs alone (pass 2: a remade pipeline no Rerun proto re-runs)
    if !matches!(g.form, GroupForm::Hashed | GroupForm::Sorted) || g.two_pass.is_some() || g.set.is_some() || eighths <= SPLIT_FLOOR_EIGHTHS || crate::rerun::sets::in_owner_set(gx, k) {
        return None;
    }
    // D452: pass 2 folds no row of a group under the floor, where PG folds every row: a fold whose row step can raise
    // (or, as the float average, defers that raise to its final: D432) keeps one pass
    if !rec.aggs.iter().all(|a| crate::effects::visible::topn::fold_quiet(&a.d, true)) {
        return None;
    }
    // a union arm's pipeline is never remade (Px::remake)
    if px.union_of(k, gx.pipelines()[k].ops.len() + 1).is_some() {
        return None;
    }
    let q = gx.drainers(g).first().copied()?;
    // PSP2-43: pass 1's reader reads K's word as an int (`Rule`), never a numeric's pointer
    let (floor, lit) = crate::ops::sort::topn::strategies::twopass_having::floor_at(px, q, rec).filter(|f| !f.0.num)?;
    // pass 1 keeps the row count, never under a count final: a floor on any count reads it, a ceiling only on count(*)'s
    let rows = floor.count && (!floor.below || rec.aggs.iter().any(|a| a.d.slot == floor.slot && a.d.class == Class::CountStar && a.d.filter.is_none()));
    // exec prices a count's bound per execution (`having_cost`); another final's is priced here, on its literal K
    if !floor.count && !bound_pays(px, g, rec, &floor, lit, eighths) {
        return None;
    }
    // D-103-price: no row estimate keeps the split, as before
    if !second_read_pays(px, g, eighths, true) {
        return None;
    }
    let (rec1, off) = match rows {
        true => {
            let rec1 = GroupRec::counts(px, g)?;
            let off = rec1.layout.sentinel_off();
            (rec1, off)
        }
        false => {
            let (rec1, j) = GroupRec::bounded(px, g, floor.slot)?;
            let off = rec1.layout.slot_off(j);
            (rec1, off)
        }
    };
    let node = gx.pipelines()[k].node_id;
    let n = having_estimate(px, g, eighths, rec1.layout.size(), floor.count);
    let rule = Rule { floor: true, gt: floor.gt, int4: floor.int4, below: floor.below, n, ..Rule::of(&rec1, off, node) };
    Some(split_by(px, k, g, rec1, &rule, Some(floor.word)))
}

/// The split of graph pipeline `k`'s group `g` under `rule` (its bound word `bound`): pass 1 (`k`) folds `rec1` into a
/// new slot, pass 2 (`k`'s scan remade right after it) folds the groups the rule admits into the group's own slot.
pub(crate) fn split_by<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, g: &'g GroupDecl<'p>, rec1: GroupRec<'g, 'p>, rule: &Rule, bound: Option<u16>) -> PgBox<'g, dyn EndMech + 'g> {
    let s1 = px.new_slot();
    let v = px.pass_id();
    let (source, steps) = px.remake(k, v);
    px.ext_mut::<KeyedExt>(Face::Keyed).homes.push((g.slot, g.producer.0, LifeId(v as u16))); // alloc: Program
    let word = declare(px, v, k, s1, rule, bound);
    let admit = Admit { word, key: None, null_word: false, every_window: false };
    let end = px.making(k, v, |px| {
        let rec2 = GroupRec::new(px, g);
        crate::frame::iface::group_end_of(px, k, rec2, Some(admit), Some(Split::Two(s1, LifeId(k as u16))), None)
    });
    px.add_pass(k, ExtraPass { source, steps, end });
    crate::frame::iface::group_end_of(px, k, rec1, None, Some(Split::One(Some(s1))), None)
}

/// Whether two passes pay for a bound on a min / max / sum final (not a count's: exec prices those per execution), decided
/// on the qual's literal K (`lit_decide`: a statement of the same key whose K decides otherwise takes the program for
/// its own outcome) from the bounded column's value range (`Facts::att_estimate`: the zone extrema on pgrc, the
/// pg_statistic bounds on heap) and the mean group size (`having_estimate`'s bucket). A parameter K, an argument that
/// is no bare column of the group's scan, or no estimate keeps one pass, the bound applied at the emit and the owner.
fn bound_pays<'p>(px: &Px<'_, 'p>, g: &GroupDecl<'_>, rec: &GroupRec<'_, '_>, f: &crate::ops::sort::topn::strategies::twopass_having::Floor, lit: types_nodes::Node<'p>, eighths: u64) -> bool {
    use gravity_decompose::ops::shared::agg::Register;
    let Some(a) = rec.aggs.iter().find(|a| a.d.slot == f.slot) else { return false };
    let kind = match a.d.reg {
        Register::MaxInt => ON_GREATEST,
        Register::MinInt => ON_LEAST,
        Register::SumInt => ON_SUM,
        _ => return false,
    };
    let Some((lo, hi)) = arg_att(px.gx, g, &a.d).and_then(|(relid, attno)| px.gx.facts.att_estimate(relid, attno)) else { return false };
    let producer = &px.gx.pipelines()[g.producer.0 as usize].desc;
    let q = gravity_lower::inputs::key::decide(g.producer.0 as usize, false, producer.predicted_rows, mean_group_bucket, [g.groups, 0, 0, 0]);
    let packed = q.min(0xff) | eighths.min(0xff) << 8 | kind << 16 | (f.below as u64) << 18 | (f.gt as u64) << 19 | (f.int4 as u64) << 20;
    q != 0 && lo <= hi && gravity_lower::inputs::key::lit_decide(lit, bound_two_passes, [lo as u64, hi as u64, packed, 0]) == Some(1)
}

const ON_GREATEST: u64 = 1;
const ON_LEAST: u64 = 2;
const ON_SUM: u64 = 3;

/// The relation and attribute aggregate `d`'s one argument reads: a bare column of its group's producer scan, through
/// the producer's projection.
fn arg_att(gx: &crate::graph::Gx<'_, '_>, g: &GroupDecl<'_>, d: &gravity_decompose::ops::shared::agg::AggDecl<'_>) -> Option<(u32, i16)> {
    use types_nodes::primnodes::OUTER_VAR;
    let a = d.node.as_aggref()?;
    if a.args.len() != 1 {
        return None;
    }
    let v = crate::table::emit_group::bare(a.args.nth(0).as_target_entry()?.expr).as_var()?;
    let p = &gx.pipelines()[g.producer.0 as usize];
    let (scanrelid, relid, _) = p.source.relation()?;
    let v = match v.varno {
        OUTER_VAR => crate::table::emit_group::entry_var(&p.ops, v.varattno).unwrap_or(v),
        _ => v,
    };
    (v.varlevelsup == 0 && (v.varno == scanrelid as i32 || v.varno == OUTER_VAR)).then_some((relid, v.varattno))
}

/// `bound_pays`' decision on K's word `w`: `a` = [lo, hi, the packed mean bucket / fold weight / kind / below / gt /
/// int4]. The column's values taken uniform over [lo, hi], groups of the mean size m: a max over K fails where no row is
/// over it ((1 - P(v > K))^m of the rows), under K where some row is not under it; a min the mirror; a sum is normal
/// over m terms. Two passes pay where (count + other) x dropped > read + count + probe (exec `having_cost`'s rule).
fn bound_two_passes(w: u64, a: &[u64; 4]) -> u64 {
    use gravity_program::cost::{HAVING_COUNT as COUNT, HAVING_PROBE as PROBE};
    let (lo, hi, p) = (a[0] as i64 as f64, a[1] as i64 as f64, a[2]);
    let m = (((p & 0xff).max(1) - 1) as f64 / 4.0).exp2();
    let other = (p >> 8 & 0xff) as f64 / 8.0;
    let (kind, below, gt) = (p >> 16 & 3, p >> 18 & 1 != 0, p >> 19 & 1 != 0);
    let k = if p >> 20 & 1 != 0 { w as u32 as i32 as f64 } else { w as i64 as f64 };
    let n = hi - lo + 1.0;
    let over = |t: f64| ((hi - t) / n).clamp(0.0, 1.0);
    let under = |t: f64| ((t - lo) / n).clamp(0.0, 1.0);
    let (over_k, under_k) = (over(if gt { k } else { k - 1.0 }), under(if gt { k } else { k + 1.0 }));
    let dropped = match (kind, below) {
        (ON_GREATEST, false) => (1.0 - over_k).powf(m),
        (ON_GREATEST, true) => 1.0 - under_k.powf(m),
        (ON_LEAST, true) => (1.0 - under_k).powf(m),
        (ON_LEAST, false) => 1.0 - over_k.powf(m),
        (ON_SUM, _) => {
            let sd = (m * (n * n - 1.0) / 12.0).sqrt().max(f64::MIN_POSITIVE);
            let under_sum = normal_cdf((k - m * (lo + hi) / 2.0) / sd);
            if below { 1.0 - under_sum } else { under_sum }
        }
        _ => 0.0,
    };
    ((COUNT + other) * dropped > 1.0 + COUNT + PROBE) as u64
}

/// The standard normal CDF (Abramowitz and Stegun 7.1.26's erf, |error| < 1.5e-7).
fn normal_cdf(z: f64) -> f64 {
    let x = z.abs() / core::f64::consts::SQRT_2;
    let t = 1.0 / (1.0 + 0.327_591_1 * x);
    let poly = t * (0.254_829_592 + t * (-0.284_496_736 + t * (1.421_413_741 + t * (-1.453_152_027 + t * 1.061_405_429))));
    let erf = 1.0 - poly * (-x * x).exp();
    if z >= 0.0 { 0.5 * (1.0 + erf) } else { 0.5 * (1.0 - erf) }
}

// KEYED-02: the second read plus pass 2's admission probe, in eighths of a read (exec `having_cost`'s 1 + PROBE): the
// most a floor can spare is every fold, so a split whose folds weigh no more never pays.
pub(crate) use gravity_program::cost::HAVING_SPLIT_FLOOR_EIGHTHS as SPLIT_FLOOR_EIGHTHS;

/// Whether a split of group `g` (its folds `eighths` per row, `fold_eighths`) pays its second read at the statement's
/// estimates: the folds weigh `eighths` eighths of a read per row reaching the group (the producer's `predicted_rows`)
/// against the second read and its probe, `SPLIT_FLOOR_EIGHTHS` per row the scan reads (the relation's rows,
/// `Facts::rel_rows`); even a split sparing every fold costs more where a scan filter keeps few of the rows read. No
/// estimate (a pipeline with no predicted rows, a source that keeps no row count, as a heap relation) decides
/// `no_estimate`. Filed as a guard of the program (`key::decide`): a program serves another statement's estimates only
/// where they decide the same.
pub(crate) fn second_read_pays(px: &Px<'_, '_>, g: &GroupDecl<'_>, eighths: u64, no_estimate: bool) -> bool {
    let gx = px.gx;
    let producer = &gx.pipelines()[g.producer.0 as usize];
    let read = producer.source.relation().and_then(|(_, relid, _)| gx.facts.rel_rows(relid)).unwrap_or(0);
    gravity_lower::inputs::key::decide(g.producer.0 as usize, false, producer.desc.predicted_rows, read_pays, [read, eighths, no_estimate as u64, 0]) != 0
}

/// `second_read_pays`' decision on rows reaching the group `rows`: `args` the rows read, the folds' eighths and what no
/// estimate decides.
fn read_pays(rows: u64, args: &[u64; 4]) -> u64 {
    let [read, eighths, no_estimate, _] = *args;
    match rows == 0 || read == 0 {
        true => no_estimate,
        false => (eighths as u128 * rows as u128 > SPLIT_FLOOR_EIGHTHS as u128 * read.max(rows) as u128) as u64,
    }
}

/// KEYED-02: one aggregate's fold per input row, in eighths of a scan read, as the split's choice weighs it (fitted on a
/// 2M-row pgrcolumnar table, groups of mean 60, floors around the mean, W0 and W8). A min / max over text codes is the
/// fold the split was first taken for (two passes always before): its weight puts the crossover at the narrowest
/// measured, so one pass runs only where it is faster at every width. A min / max over a Var slot (an unscaled numeric,
/// text under a collation that does not order by bytes) was one pass before: its weight puts the crossover at the widest
/// measured, so two passes run only where they are faster at every width; one alone never splits. A fold whose row step
/// can raise never reaches the split (D452), and every other fold is one add or compare. The rule carries at most 255.
pub(crate) fn fold_eighths(a: &crate::table::rec::Agg<'_>) -> u64 {
    match a.d.trans {
        Trans::Abi(SlotKind::MinCode | SlotKind::MaxCode) if matches!(a.d.final_, FinalForm::Rendered) => gravity_program::cost::HAVING_CODE_FOLD_EIGHTHS,
        Trans::Abi(SlotKind::MinVar | SlotKind::MaxVar | SlotKind::AggState) | Trans::Var => gravity_program::cost::HAVING_VAR_FOLD_EIGHTHS,
        _ => 1,
    }
}

/// KEYED-02: the floor rule's estimate bits (its free `n`; exec ops/sort/topn/proto.rs `having_cost::unpack`): bits 0..7
/// the mean group size m = rows / groups of the group's producer as `1 + round(4 log2 m)` (0: no estimate, or a bound on
/// another final than a count (`counted` false), which the exec's count model does not price: two passes always), bits 8..15 the folds' weight in eighths of a read, bits 16..22 pass 1's table (its groups times its record,
/// filled to 7/8) as `round(2 log2 bytes)`: exec runs one pass where that outgrows one participant's share of the grant,
/// since a pass 1 that spills pays its spill (and with no owner result admits every key, D459, so pass 2 folds every row
/// after the second read). The mean is an estimate
/// outside the content key, so it is decided through `key::decide` (KEY-PREDICTED-ROWS road (b)): a program serves
/// another estimate only where the quarter-octave bucket is the same.
fn having_estimate(px: &Px<'_, '_>, g: &GroupDecl<'_>, eighths: u64, rec_bytes: u32, counted: bool) -> u32 {
    let producer = &px.gx.pipelines()[g.producer.0 as usize].desc;
    let q = match counted {
        true => gravity_lower::inputs::key::decide(g.producer.0 as usize, false, producer.predicted_rows, mean_group_bucket, [g.groups, 0, 0, 0]),
        false => 0,
    };
    (q.min(0xff) | eighths.min(0xff) << 8 | pass1_bytes_bucket(g.groups, rec_bytes) << 16) as u32
}

/// `1 + round(4 log2 (rows / groups))`, at most 255; 0 when either estimate is 0.
fn mean_group_bucket(rows: u64, args: &[u64; 4]) -> u64 {
    if rows == 0 || args[0] == 0 {
        return 0;
    }
    let m = (rows as f64 / args[0] as f64).max(1.0);
    (1.0 + (4.0 * m.log2()).round()).min(255.0) as u64
}

/// Pass 1's table in bytes, `groups × record bytes × 8 / 7` (a hashed table fills to 7/8), as `round(2 log2 bytes)`, 1 to
/// 127; 0 when either is 0 (no estimate: exec reads no size).
fn pass1_bytes_bucket(groups: u64, rec_bytes: u32) -> u64 {
    if groups == 0 || rec_bytes == 0 {
        return 0;
    }
    let bytes = (groups as f64 * rec_bytes as f64 * 8.0 / 7.0).max(1.0);
    (2.0 * bytes.log2()).round().clamp(1.0, 127.0) as u64
}

#[cfg(test)]
mod tests {
    use super::{mean_group_bucket, pass1_bytes_bucket, read_pays, Rule, SPLIT_FLOOR_EIGHTHS};

    #[test]
    fn the_mean_group_size_packs_by_quarter_octaves() {
        assert_eq!(mean_group_bucket(100, &[100, 0, 0, 0]), 1);
        assert_eq!(mean_group_bucket(7 * 1024, &[7, 0, 0, 0]), 41);
        // under one row a group (more groups than rows estimated): the mean is one
        assert_eq!(mean_group_bucket(3, &[10, 0, 0, 0]), 1);
        assert_eq!(mean_group_bucket(0, &[5, 0, 0, 0]), 0);
        assert_eq!(mean_group_bucket(5, &[0, 0, 0, 0]), 0);
        assert_eq!(mean_group_bucket(u64::MAX, &[1, 0, 0, 0]), 255);
        // a floor rule carries it in its n, below the fold weight and pass 1's table, through pack / unpack
        let r = Rule { floor: true, gt: true, n: 41 | 32 << 8 | 127 << 16, ..Rule::default() };
        assert_eq!(Rule::unpack(r.pack(Some(3))).n, 41 | 32 << 8 | 127 << 16);
    }

    #[test]
    fn a_bound_on_a_column_range_pays_two_passes_only_where_it_drops_the_rows() {
        use super::{bound_two_passes, ON_GREATEST, ON_LEAST, ON_SUM};
        // values 0 ..= 49999, groups of about 233 rows (bucket 1 + round(4 log2 233) = 32), costly folds (30 reads)
        let args = |kind: u64, below: bool, gt: bool| [0, 49_999, 32 | 240 << 8 | kind << 16 | (below as u64) << 18 | (gt as u64) << 19, 0];
        // max(v) > 49990: few groups reach it; max(v) > 0: every group does
        assert_eq!(bound_two_passes(49_990, &args(ON_GREATEST, false, true)), 1);
        assert_eq!(bound_two_passes(0, &args(ON_GREATEST, false, true)), 0);
        // min(v) < 10 drops most groups, min(v) <= 49999 none; max(v) < 49000 drops nearly all, min(v) > 0 nearly none
        assert_eq!(bound_two_passes(10, &args(ON_LEAST, true, true)), 1);
        assert_eq!(bound_two_passes(49_999, &args(ON_LEAST, true, false)), 0);
        assert_eq!(bound_two_passes(49_000, &args(ON_GREATEST, true, true)), 1);
        assert_eq!(bound_two_passes(0, &args(ON_LEAST, false, true)), 0);
        // a sum over 215 terms (the bucket's mean) of mean 24999.5 (5.37M, sd ~212k): >= 7M drops nearly every group,
        // >= 4.5M almost none, < 4.5M nearly all
        assert_eq!(bound_two_passes(7_000_000, &args(ON_SUM, false, false)), 1);
        assert_eq!(bound_two_passes(4_500_000, &args(ON_SUM, false, false)), 0);
        assert_eq!(bound_two_passes(4_500_000, &args(ON_SUM, true, true)), 1);
        // an int4 K's word reads its low half, sign-extended
        let a = [(-100i64) as u64, 100, 32 | 240 << 8 | ON_LEAST << 16 | 1 << 18 | 1 << 19 | 1 << 20, 0];
        assert_eq!(bound_two_passes((-99i32) as u32 as u64 | 0xdead << 32, &a), 1);
        // cheap folds never pay
        assert_eq!(bound_two_passes(49_990, &[0, 49_999, 32 | 1 << 8 | ON_GREATEST << 16 | 1 << 19, 0]), 0);
    }

    #[test]
    fn the_split_pays_on_the_rows_reaching_the_group_against_the_rows_read() {
        let heavy = 481; // two text extremes over codes and the count
        for none in [0, 1] {
            assert_eq!(read_pays(4_000_000, &[4_000_000, heavy, none, 0]), 1, "no filter: the folds outweigh the second read");
            assert_eq!(read_pays(1_600_000, &[4_000_000, heavy, none, 0]), 1, "a filter keeping 40 %");
            assert_eq!(read_pays(20_000, &[4_000_000, heavy, none, 0]), 0, "a filter keeping 0.5 %: the second read costs more");
            assert_eq!(read_pays(4_000_000, &[4_000_000, SPLIT_FLOOR_EIGHTHS, none, 0]), 0, "folds at the floor never pay");
            // no row estimate, no relation count: what the caller asks (twopass.count one pass, twopass.having the split)
            assert_eq!(read_pays(0, &[4_000_000, heavy, none, 0]), none);
            assert_eq!(read_pays(4_000_000, &[0, heavy, none, 0]), none);
        }
    }

    #[test]
    fn pass_1s_table_packs_by_half_octaves() {
        // 2^20 groups of a 24-byte record, filled to 7/8: 28.8 MB, 2 log2 = 49.6
        assert_eq!(pass1_bytes_bucket(1 << 20, 24), 50);
        assert_eq!(pass1_bytes_bucket(1, 7), 6);
        assert_eq!(pass1_bytes_bucket(0, 24), 0);
        assert_eq!(pass1_bytes_bucket(5, 0), 0);
        assert_eq!(pass1_bytes_bucket(u64::MAX, u32::MAX), 127);
    }

    #[test]
    fn a_rule_packs_into_the_shape_bits_and_its_bound_word_above_them() {
        let r = Rule { floor: true, gt: true, int4: true, below: true, desc: false, nulls_first: true, nullable: false, ext: false, ext_nn: false, spread: false, img_words: 2, rec_words: 7, count_word: 4, n: 0x2b_cdef, top_count: false, lead: 0 };
        let a = r.pack(Some(41));
        assert_eq!(Rule::unpack(a), r);
        assert_eq!((a >> 48 & 0x7fff, a >> 63), (42, 1));
        assert_eq!(Rule::unpack(r.pack(None)), r);
        assert_eq!(r.pack(None) >> 48 & 0x7fff, 0);
    }

    #[test]
    fn an_extreme_rule_packs_into_the_floor_bits_it_never_uses() {
        let r = Rule { ext: true, ext_nn: true, desc: true, nulls_first: true, nullable: true, img_words: 2, rec_words: 5, count_word: 2, n: 300, ..Rule::default() };
        assert_eq!(Rule::unpack(r.pack(None)), r);
        let r = Rule { ext: true, ..r };
        assert_eq!(Rule::unpack(r.pack(None)), r);
        let f = Rule { floor: true, gt: true, int4: false, ..Rule::default() };
        let u = Rule::unpack(f.pack(Some(3)));
        assert!(u.gt && !u.ext && !u.ext_nn, "a floor's bits stay the floor's");
        let u = Rule::unpack(Rule { below: true, ..f }.pack(Some(3)));
        assert!(u.below && !u.spread, "a floor's bit 63 is its below");
    }

    #[test]
    fn a_top_count_rule_packs_its_lead_into_the_extreme_bits() {
        for lead in 0..=6u8 {
            let r = Rule { top_count: true, lead, desc: lead % 2 == 0, nulls_first: lead > 2, img_words: 1, rec_words: 4, count_word: 1, n: 0x7f_ffff, ..Rule::default() };
            let u = Rule::unpack(r.pack(None));
            assert_eq!(u, r);
            assert!(!u.ext && !u.ext_nn && !u.spread && !u.floor, "a lead is never an extreme's bits");
            assert_eq!(r.pack(None) >> 48 & 0x7fff, 0, "no bound word");
        }
    }
}


//! Road `run.pergroup` (cards/M3.md L6, H26 d; V4R-147): DISTINCT ON over an input ordered by its keys. decompose maps a
//! Unique whose output reads a column outside its keys to `Op::FirstOfRun` over the ordered input (agg.rs map_set,
//! ops.rs `first_of_run`: the plan's `uniqColIdx` / `uniqOperators`, the input's proven order leading with them), and
//! runs.unique's step face lands here: a serial step whose frame words hold the last passed row's key images, their NULL
//! flags and a seen flag. A row passes when no row has passed yet or a key differs from the last passed row's (two NULLs
//! equal, as PostgreSQL's Unique); the rest of the run drops. The row's steps below (the projection PostgreSQL evaluates
//! under the Sort for every input row) ran before this step, so a raising expression raises on a dropped row too. A
//! row's keys are stored once it is through the steps above (frame.carry's `through` protocol, D357: a stop that
//! leaves past the row stores them, one that resumes at the row stores nothing). Keys (D1517): the
//! plan operator is the type's own `=` (TypeFacts.eq) and the key's exact image by its hash arm decides, the keyed groups'
//! rule: a word image only when it is the type's equality (floats' canonical -0 = +0, one NaN), text / numeric / fixed /
//! hosted / code by their arm (an arm not built refuses by its name), a nondeterministic collation by key.coll's sort-key
//! Fp128 (face/keyimg.rs). A parallel or union pipeline refuses by Shape (serial only). Owner M3-GROUPED. Budget 160 lines.
use gravity_abi::status::{unsupported, UNSUPPORTED_SHAPE};
use gravity_decompose::catalog::typefacts::{self, KeyColl, TypeFacts};
use gravity_decompose::Op;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type, Value};

use crate::table::keyimg::{ImageForm, VarKeyImpl};
use crate::face::kinds::{Face, KindId};
use crate::face::step::StepMech;
use crate::frame::env::Env;
use crate::frame::frame::{Next, StepMaker};
use crate::frame::open::{Base, OpenWord, OpenWords, Scope};
use crate::frame::px::{Px, WordKey};
use crate::frame::row::{Carry, RowVals};
use crate::admit::refusal::{Reason, ShapeWhy, Variant};
use crate::ops::Table;
use crate::rep::{self, key_kind, Accessor};
use gravity_decompose::catalog::keysem::image::class_image;
use mcx::{Mcx, PgBox, PgVec};
use mcx::GrowIn;
use mcx::CollectIn;

crate::strategies! {
    /// Bounded run per group (DISTINCT ON, LIMIT per group).
    RUN_PERGROUP = 72, "run.pergroup", DistinctOn;
}

pub(crate) const ROWS: Table = Table { strategies: &[(RUN_PERGROUP, &StepMaker(make))], ..crate::ops::NONE };

/// A run key: the input column, its type's facts and the frame words of the last passed row's image and NULL flag.
struct Key {
    resno: i16,
    tf: TypeFacts,
    coll: KeyColl,
    img: [i32; 2],
    null: i32,
    /// KeySem slice 4 (group): a key under an equality other than its type's own, compared through it (`other_eq`; `None`:
    /// by its image).
    eq: Option<OtherEq>,
}

/// `other_eq`'s key: `proc | coll << 32` for stream.rs's host, and for a fixed-length by-reference key the frame words that
/// hold the last passed row's datum bytes (the first's byte offset, the count; contiguous).
#[derive(Clone, Copy)]
struct OtherEq {
    pc: u64,
    copy: Option<(i32, u16)>,
}

struct FirstOfRun<'g> {
    keys: PgVec<'g, Key>,
    /// Set once a row has passed.
    seen: i32,
    node: i32,
    mcx: Mcx<'g>,
}

fn make<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let mcx = px.mcx();
    let p = &px.gx.pipelines()[k];
    let node = p.node_ops.iter().rev().find(|n| n.first_op as usize <= j).map_or(p.node_id, |n| n.node_id);
    let Op::FirstOfRun(t) = &p.ops[j] else { crate::refuse!(node, Reason::Bug { payload: "a first-of-run entry over another op" }) };
    if p.desc.is_eligible() || px.union_of(k, j).is_some() {
        crate::refuse!(node, Reason::Shape { why: ShapeWhy("run.pergroup: DISTINCT ON in a parallel or union pipeline (serial only)") });
    }
    let mut keys = mcx::reserved_in(t.keys.len(), mcx);
    for (n, key) in t.keys.iter().enumerate() {
        // the key's hash arm by its type, as props/ops.rs names it (D1517: any built arm; a word image only as the type's `=`)
        let (tf, coll) = (typefacts::facts_of(px.gx.facts, key.typid), px.gx.facts.coll(t.colls.get(n).copied().unwrap_or(0)));
        let need = key_kind(key.kind, tf.map(|tf| class_image(tf.image)), false, Accessor::Hash);
        if !crate::ops::built_kinds().has(need) { crate::not_built!(node, Variant::Kind(need)) }
        let Some(tf) = tf.filter(|tf| need != KindId::RepWordHash || tf.image.equality()) else {
            crate::refuse!(node, Reason::Shape { why: ShapeWhy("run.pergroup: a DISTINCT ON word key whose image is not its type's equality") })
        };
        // D4126: an anonymous record key whose ROW() columns the plan knows (decompose TieDesc `rows`) images by their
        // canonical form, as a keyed group's record key; one known only per row keeps rep.hosted.hash's refusal
        let tf = match t.rows.get(n) {
            Some(cols) if !cols.is_empty() => crate::rep::hosted::hash::record_form_tf(node, tf, false, Some(&cols[..])),
            _ => tf,
        };
        // KeySem slice 4 (keysem::eq::own_eq): the key's `=` is its type's own (a failed read compares through the operator)
        let own = {
            let mut cx = mcx.context().new_child_bump("run.pergroup own ="); // PLANMEM: under the planning context (its owner)
            let r = crate::effects::EqReads(px.gx.catalog);
            let own = gravity_fn::typesem::keysem::eq::own_eq(cx.mcx(), &r, key.opno, key.typid, tf.eq).unwrap_or(false);
            cx.reset();
            own
        };
        let eq = (!own).then(|| {
            let pc = other_eq(px, node, key.opno, &tf, t.colls.get(n).copied().unwrap_or(0));
            let copy = (!tf.byval).then(|| {
                let words = tf.typlen as u16 / 8;
                let at: PgVec<'_, u16> = (0..words).map(|i| px.frame_word(WordKey::Open { face: Face::Step, k, j: crate::frame::px::key_j(j) ^ COPY_TAG ^ (n as u64) << 40, i: i as u8 })).collect_in(mcx);
                if at.windows(2).any(|w| w[1] != w[0] + 1) {
                    crate::refuse!(node, Reason::Bug { payload: "a DISTINCT ON key copy's frame words are not contiguous" });
                }
                (at[0] as i32 * 8, words)
            });
            OtherEq { pc, copy }
        });
        // CAP-30 (decompose passes more than eight keys over a Sort's emit): each key's three words from index 1, 85 keys a
        // chunk, a chunk past the step's own `u8` indices keyed past the op's position (bit 56 up, as ops/limit/lower.rs's
        // tie words), so a wrapped index never hands one word two roles (D2241's class)
        let (c, i) = (n / 85, (1 + 3 * (n % 85)) as u8);
        let mut word = |i: u8| px.frame_word(WordKey::Open { face: Face::Step, k, j: crate::frame::px::key_j(j) ^ ((c as u64) << 56), i }) as i32 * 8;
        keys.push_in(Key { resno: key.resno, tf, coll, img: [word(i), word(i + 1)], null: word(i + 2), eq });
    }
    let seen = px.frame_word(WordKey::Open { face: Face::Step, k, j: crate::frame::px::key_j(j), i: 0 }) as i32 * 8;
    mcx::pg_box_dyn!(mcx, FirstOfRun { keys, seen, node, mcx } => dyn StepMech + 'g)
}

/// `OtherEq::copy`'s frame words: keyed apart from the keys' own words (`j`'s bit 55, the key's index from bit 40).
const COPY_TAG: u64 = 1 << 55;

/// KeySem slice 4 (group; eq = Call, site = Leader): a key under an equality other than its type's own (a user `=`, or a
/// built-in one that is not the type's: box's area `=` under a user btree class, K1.boxd) compares the last passed row's
/// key with the row's through that `=`, as PostgreSQL's execTuplesMatch: bound at lowering by KeySem (`keysem::eq::eq_call`:
/// a user function runs on the leader, a SQL / PL one's statements served nested there, D3375 N7), its id and
/// the key's collation for the host. The last passed row's key is its image word when that word is its datum (a by-value
/// integer or OID key), else a copy of its datum bytes in frame words (a fixed-length by-reference key of whole words, at
/// most 128 bytes); any other key refuses by name.
fn other_eq(px: &Px<'_, '_>, node: i32, op: u32, tf: &TypeFacts, coll: u32) -> u64 {
    let word = tf.byval && matches!(tf.typlen, 2 | 4 | 8) && (tf.image.is_word() || tf.image.is_enum());
    let fixed = !tf.byval && tf.typlen > 0 && tf.typlen % 8 == 0 && tf.typlen <= 128 && tf.rec == 0;
    if !word && !fixed {
        crate::refuse!(node, Reason::Shape { why: ShapeWhy("run.pergroup: a DISTINCT ON key under an equality other than its type's own that is neither a datum word nor whole fixed-length words") });
    }
    use gravity_fn::typesem::keysem::eq::{eq_call, EqCallErr};
    let id = match eq_call(&crate::effects::EqReads(px.gx.catalog), op, tf.typid) {
        Ok(id) => id,
        Err(EqCallErr::NoProc) => crate::refuse!(node, Reason::Shape { why: ShapeWhy("run.pergroup: a DISTINCT ON key's equality whose procedure the catalog does not give") }),
        Err(EqCallErr::Unbound(why)) => crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) }),
    };
    id as u64 | (coll as u64) << 32
}

impl<'g> FirstOfRun<'g> {
    /// `same` (a row has passed and every image key matches) and, last key first as ExecBuildGroupingEqual, each `other_eq`
    /// key through its `=`: called only while the row may still be in the run and neither key is NULL (two NULLs are equal,
    /// a NULL and a value differ); a NULL result is not equal, a raise leaves through `l_raise`.
    fn other_same(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, keys: &[([Option<Value>; 2], Value)], mut same: Value) -> Value {
        let st = self.words(env);
        let mut host = None;
        for (t, (w, null)) in self.keys.iter().zip(keys.iter()).rev() {
            let Some(OtherEq { pc, copy }) = t.eq else { continue };
            let (Some(v), None) = (w[0], w[1]) else { crate::refuse!(self.node, Reason::Bug { payload: "a DISTINCT ON key under another = without its datum" }) };
            let h = *host.get_or_insert_with(|| crate::ops::group::stream::eq_host(b));
            let sn = b.load(Type::I64, st, t.null);
            let snull = b.icmp(IntCond::Ne, sn, env.zero);
            let both_null = b.int_bin(IntBinOp::And, snull, *null);
            let either = b.int_bin(IntBinOp::Or, snull, *null);
            let t1 = b.bconst(true);
            let neither = b.int_bin(IntBinOp::Xor, either, t1);
            let when = b.int_bin(IntBinOp::And, same, neither);
            let s = match copy {
                Some((off, _)) => {
                    let p = b.ptr_add(st, env.zero, 1, off);
                    b.conv(gravity_ir::ConvOp::PtrToInt, Type::I64, p)
                }
                None => b.load(Type::I64, st, t.img[0]),
            };
            let pc = b.const_raw(Type::I64, pc);
            let r = crate::ops::group::stream::eq_call(b, env, h, when, &[env.ctx, s, v, pc], env.l_raise);
            let r = b.icmp(IntCond::Ne, r, env.zero);
            let valeq = b.int_bin(IntBinOp::And, neither, r);
            let k_eq = b.int_bin(IntBinOp::Or, both_null, valeq);
            same = b.int_bin(IntBinOp::And, same, k_eq);
        }
        same
    }

    fn words(&self, env: &Env) -> Value {
        match env.words {
            Some(w) => w,
            None => crate::refuse!(self.node, Reason::Bug { payload: "DISTINCT ON without the frame words" }),
        }
    }

    /// The row's key images and NULL flags: `(words, null)` per key; a key whose image is not exact refuses.
    fn keys(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals) -> PgVec<'g, ([Option<Value>; 2], Value)> {
        let mut cx = env.lx(b, self.node);
        let mut out = mcx::reserved_in(self.keys.len(), self.mcx);
        for t in self.keys.iter() {
            let Some(c) = row.col(t.resno) else { crate::refuse!(self.node, Reason::Bug { payload: "a DISTINCT ON key the row does not carry" }) };
            // a copied key: its datum (the pointer `other_same` hands the host), no image
            if t.eq.is_some_and(|e| e.copy.is_some()) {
                let (p, null) = rep::datum(&mut cx, c, &t.tf);
                let p = cx.b.conv(gravity_ir::ConvOp::PtrToInt, Type::I64, p);
                out.push_in(([Some(p), None], null));
                continue;
            }
            let w = if t.coll.bytes_eq { rep::hash_image(&mut cx, c, &t.tf, rep::Scope::Program) } else { ImageForm::Coll.need::<VarKeyImpl>(&cx).0.hash(&mut cx, c, &t.tf, t.coll, rep::Scope::Program).unwrap_or_else(|| crate::refuse!(self.node, Reason::Bug { payload: "key.coll declined a DISTINCT ON key" })) }; // D1517: keyimg.rs coll_image
            if !w.exact {
                crate::refuse!(self.node, Reason::Shape { why: ShapeWhy("run.pergroup: a DISTINCT ON key whose image is not exact") });
            }
            let null = match c.null {
                Some(n) => n,
                None => cx.b.bconst(false),
            };
            out.push_in((w.w, null));
        }
        env.memo = cx.memo;
        out
    }
}

impl<'g> StepMech for FirstOfRun<'g> {
    fn open_words(&self, w: &mut OpenWords) {
        let mut all = mcx::pg_vec![in self.mcx; ("first-of-run seen", self.seen)];
        for t in self.keys.iter() {
            all.extend_in([("first-of-run image", t.img[0]), ("first-of-run image", t.img[1]), ("first-of-run null", t.null)]);
            if let Some((off, n)) = t.eq.and_then(|e| e.copy) {
                all.extend_in((0..n as i32).map(|i| ("first-of-run key copy", off + 8 * i)));
            }
        }
        for (what, off) in all {
            w.declare(OpenWord { what, base: Base::Words, off, ty: Type::I64, init: 0, scope: Scope::Open });
        }
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        let st = self.words(env);
        let keys = self.keys(b, env, row);
        let seen = b.load(Type::I64, st, self.seen);
        let mut same = b.icmp(IntCond::Ne, seen, env.zero);
        for (t, (w, null)) in self.keys.iter().zip(keys.iter()).filter(|(t, _)| t.eq.is_none()) {
            let sn = b.load(Type::I64, st, t.null);
            let snull = b.icmp(IntCond::Ne, sn, env.zero);
            let mut eq = b.bconst(true);
            for (i, v) in w.iter().enumerate() {
                if let Some(v) = v {
                    let s = b.load(Type::I64, st, t.img[i]);
                    let e = b.icmp(IntCond::Eq, s, *v);
                    eq = b.int_bin(IntBinOp::And, eq, e);
                }
            }
            // two NULLs are equal; a NULL and a value differ; two values compare by image
            let both_null = b.int_bin(IntBinOp::And, snull, *null);
            let either = b.int_bin(IntBinOp::Or, snull, *null);
            let t1 = b.bconst(true);
            let neither = b.int_bin(IntBinOp::Xor, either, t1);
            let valeq = b.int_bin(IntBinOp::And, neither, eq);
            let k_eq = b.int_bin(IntBinOp::Or, both_null, valeq);
            same = b.int_bin(IntBinOp::And, same, k_eq);
        }
        same = self.other_same(b, env, &keys[..], same);
        let types: PgVec<'_, Type> = row.carry.params().iter().map(|&v| b.value_type(v)).collect_in(self.mcx);
        let (l_cont, l_drop, l_pass) = (b.create_block(&types[..]), b.create_block(&[]), b.create_block(&[]));
        b.cond_br(same, (l_drop, &[]), (l_pass, &[]));
        b.switch_to(l_drop);
        b.br(l_cont, &row.carry.params());
        b.switch_to(l_pass);
        let one = env.one;
        let store = |b: &mut FunctionBuilder<'_>| {
            for (t, (w, null)) in self.keys.iter().zip(keys.iter()) {
                match t.eq.and_then(|e| e.copy) {
                    Some((off, n)) => copy_key(b, st, off, n, w[0], *null),
                    None => {
                        for (i, v) in w.iter().enumerate() {
                            if let Some(v) = v {
                                b.store(st, t.img[i], *v);
                            }
                        }
                    }
                }
                let nw = b.conv(gravity_ir::ConvOp::Zext, Type::I64, *null);
                b.store(st, t.null, nw);
            }
            b.store(st, self.seen, one);
        };
        let c2 = through(b, env, row, next, &store, self.mcx);
        store(b);
        b.br(l_cont, &c2.params());
        b.switch_to(l_cont);
        Carry::from_params(b.block_params(l_cont), env.mcx)
    }
}

/// A copied key's datum bytes (`n` words at `datum`) into the frame words at `off`; a NULL key copies nothing (its NULL
/// word says so and `other_same` never reads the copy then).
fn copy_key(b: &mut FunctionBuilder<'_>, st: Value, off: i32, n: u16, datum: Option<Value>, null: Value) {
    let Some(d) = datum else { return };
    let (l_copy, l_on) = (b.create_block(&[]), b.create_block(&[]));
    b.cond_br(null, (l_on, &[]), (l_copy, &[]));
    b.switch_to(l_copy);
    let p = b.conv(gravity_ir::ConvOp::IntToPtr, Type::Ptr, d);
    for i in 0..n as i32 {
        let w = b.load(Type::I64, p, 8 * i);
        b.store(st, off + 8 * i, w);
    }
    b.br(l_on, &[]);
    b.switch_to(l_on);
}

/// frame.carry's `through` (D357), for a step outside M1: `next` for a passed row, `store` once the row is through. A
/// stop inside `next` past the row stores; one at the row stores nothing (the resume re-enters it); any other cursor
/// leaves with UNSUPPORTED_SHAPE, never a lost or doubled row.
fn through<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>, store: &dyn Fn(&mut FunctionBuilder<'_>), mcx: Mcx<'_>) -> Carry<'e> {
    let l_seen = b.create_block(&[Type::I64, Type::I64, Type::I64]);
    b.mark_cold(l_seen);
    let stop_in = core::mem::replace(&mut env.x.l_stop_in, l_seen);
    let c = next(b, env, row);
    env.x.l_stop_in = stop_in;
    let here = b.current();
    b.switch_to(l_seen);
    let p = mcx::to_vec_in(&b.block_params(l_seen)[..], mcx);
    let past = b.icmp(IntCond::Eq, p[1], row.next);
    let at = b.icmp(IntCond::Eq, p[1], row.at);
    let known = b.int_bin(IntBinOp::Or, past, at);
    let (l_known, l_bad, l_store, l_on) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
    b.cond_br(known, (l_known, &[]), (l_bad, &[]));
    b.switch_to(l_bad);
    let uns = b.iconst(Type::I64, unsupported(UNSUPPORTED_SHAPE));
    b.br(env.x.l_exit, &[uns]);
    b.switch_to(l_known);
    b.cond_br(past, (l_store, &[]), (l_on, &[]));
    b.switch_to(l_store);
    store(b);
    b.br(l_on, &[]);
    b.switch_to(l_on);
    b.br(stop_in, &p[..]);
    b.switch_to(here);
    c
}

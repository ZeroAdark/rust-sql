//! The join keys both sides share (join.inner's build end, build.rs, and its probe, probe_inner.rs): a key's source over
//! its side's row, the key pairs' type check, the row's key cells, their image (KeyHash's words) and their NULL flag.
//! Budget 120 lines. Owner M1-JOIN.
use gravity_decompose::catalog::Facts;
use gravity_decompose::join::{JoinDesc, JoinKeyDecl, KeyRaise};
use gravity_decompose::Op;
use gravity_decompose::catalog::typefacts::{KeyColl, TypeFacts};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, IntBinOp, Type, Value};
use gravity_program::{NumTypmod, Rep, Until};
use types_nodes::primnodes::{INNER_VAR, OUTER_VAR};
use types_nodes::Node;

use mcx::{CollectIn, GrowIn, PgVec};

use crate::cell::{At, Cell, CellHome, CellLife, TIDOID};
use crate::table::keyimg;
use crate::table::keyimg::KeyCell;
use crate::face::kinds::KindId;
use crate::frame::env::Env;
use crate::frame::px::{ExId, Px};
use crate::frame::row::RowVals;
use crate::admit::refusal::{Reason, ShapeWhy, Variant};
use crate::rep::{CellRef, Lx, Scope};

use gravity_abi::limits::JOIN_KEY_IMAGE_WORDS;
use crate::ops::hashjoin::keys_name;

pub(crate) use crate::effects::visible::hashjoin::keys_quiet;

/// A key's source over its side's row: a column of the row, or an expression (a by-reference result: keys_byref.rs); with the key type's
/// catalog facts (`step_expr::tf_of`, V4R-84), which decide its image, and the clause's collation (D823: key.coll's).
#[derive(Clone, Copy)]
pub(crate) enum KeySrc {
    Col(i16, TypeFacts, KeyColl),
    Ex(ExId, TypeFacts, KeyColl),
    /// D1899: an expression key that acts (a cast to a domain whose CHECK calls VOLATILE), its value held across a stop
    /// inside the row by the probe's own offset word (the byte offset): a resumed row reads it, as PG hashes a row once.
    Held(ExId, TypeFacts, KeyColl, i32),
    /// A name key beside a text key (keys_name.rs): a column or an expression of a name, read as its text.
    NameCol(i16, TypeFacts, KeyColl),
    NameEx(ExId, TypeFacts, KeyColl),
}

/// D1899: probe `keys` whose expression holds its value (`Ex::held`: it acts) read it back on the row a stop inside the
/// probe's matches re-enters (`offset`, its StepOffset word): PG computes an outer row's hash key once
/// (ExecHashJoinOuterGetTuple), where the re-entered row would run its key, and the CHECK, again.
pub(crate) fn held_by<'g>(px: &Px<'g, '_>, keys: PgVec<'g, KeySrc>, offset: i32) -> PgVec<'g, KeySrc> {
    keys.into_iter().map(|k| match k {
        KeySrc::Ex(id, tf, c) if px.placed(id).held.len() >= 2 => KeySrc::Held(id, tf, c, offset),
        k => k,
    }).collect_in(px.mcx())
}

/// Key `n` of pair `d` (its side's half): a name beside a text keys as its text (keys_name.rs).
pub(crate) fn key_src<'p>(px: &mut Px<'_, 'p>, n: Node<'p>, id: i32, d: &JoinKeyDecl<'_>) -> KeySrc {
    let coll = d.key.coll;
    let mut s = n.as_target_entry().map_or(n, |te| te.expr);
    // D3630: a relabel that re-forms the word through a domain (`d::int4` over a domain over oid) is computed, not read
    while let Some(a) = gravity_decompose::expr::through_relabel_in(px.gx.facts, s) {
        s = a;
    }
    let tf = crate::step::expr::tf_of(px.gx, crate::rep::expr_type::result_type(px.gx, n, id));
    let name = keys_name::is_name(&tf) && gravity_decompose::catalog::keysem::image::name_text(px.gx.facts, d.key.types, coll);
    match s.as_var() {
        Some(v) if v.varattno > 0 && (v.varno == OUTER_VAR || v.varno > 0) && v.varno != INNER_VAR => match name {
            true => KeySrc::NameCol(v.varattno, tf, coll),
            false => KeySrc::Col(v.varattno, tf, coll),
        },
        // EXPR-ODDS C: a by-reference result keys as its datum (join/keys_byref.rs)
        _ => match (crate::step::expr::compile(px, n, None, id), name) {
            (ex, true) => KeySrc::NameEx(ex, tf, coll),
            (ex, false) => KeySrc::Ex(ex, tf, coll),
        },
    }
}

/// Each key pair's words can be made equal (decompose join.rs `images_pair`): one type, two integer types (their Word
/// images are the value), two types of one representation and equality (a binary-coercible relabel, regproc = oid, a
/// domain = its base), or a name beside a text (keys_name.rs: the name keys as its text). D2725: any other pair under a
/// builtin hashable equality keys by its hash values (decompose `user_keys`) where the facts read hash functions.
pub(crate) fn check_keys(facts: &dyn Facts, id: i32, decls: &[JoinKeyDecl<'_>], nkeys: usize) {
    if decls.len() != nkeys {
        crate::refuse!(id, Reason::Bug { payload: "a hash join's key decls are not its keys" });
    }
    for d in decls {
        let (a, _) = d.key.types;
        if d.key.kind.is_none() {
            crate::refuse!(id, Reason::NoHash { typid: a });
        }
        if !gravity_decompose::catalog::keysem::image::images_pair(facts, d.key.types, d.key.coll) {
            crate::refuse!(id, Reason::Shape { why: ShapeWhy("a cross-type join key") });
        }
    }
}

pub(crate) type KeyVals<'e> = PgVec<'e, (Cell, Value, Option<Value>, TypeFacts)>;

/// The key cells of a row: a column's cell as it is, an expression's value as a Word cell. D1898: with `stop` (a side that
/// does not keep NULL keys, PG's strict hash: `ExecBuildHash32Expr` with `keep_nulls` false) a key that may be NULL branches
/// to `stop` with its arguments before the next key is emitted, so a later key's errors and calls never happen on that row;
/// the caller still branches on the last key's flag (`any_null`).
pub(crate) fn key_cells<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, node: i32, row: &RowVals, keys: &[KeySrc], stop: Option<(Block, &[Value])>) -> KeyVals<'e> {
    let t = b.bconst(true);
    let mut cells: KeyVals<'e> = mcx::reserved_in(keys.len(), env.mcx);
    for (i, k) in keys.iter().enumerate() {
        let c = match *k {
            KeySrc::Col(a, tf, _) | KeySrc::NameCol(a, tf, _) => match row.col(a) {
                Some(c) => (*c.cell, c.val, c.null, tf),
                None => crate::refuse!(node, Reason::Shape { why: ShapeWhy("a join key the row does not carry") }),
            },
            KeySrc::Ex(id, tf, _) | KeySrc::NameEx(id, tf, _) => {
                let (v, valid) = crate::step::expr::emit(b, env, node, row, id);
                (crate::ops::hashjoin::keys_byref::expr_cell(&tf), v, Some(b.int_bin(IntBinOp::Xor, valid, t)), tf)
            }
            KeySrc::Held(id, tf, _, own) => {
                let at = &env.placed(id).held[..];
                let h = crate::frame::stop::held(b, env, &at[..2], Some(own), &mut |b, env| {
                    let (v, valid) = crate::step::expr::emit(b, env, node, row, id);
                    mcx::pg_vec![in env.mcx; v, valid]
                });
                (crate::ops::hashjoin::keys_byref::expr_cell(&tf), h[0], Some(b.int_bin(IntBinOp::Xor, h[1], t)), tf)
            }
        };
        if let (Some((l_stop, args)), Some(null), true) = (stop, c.2, i + 1 < keys.len()) {
            let l_go = b.create_block(&[]);
            b.cond_br(null, (l_stop, args), (l_go, &[]));
            b.switch_to(l_go);
        }
        // D2849: PG hashes the key here, before the next key, unless it is NULL; a hash under no collation raises
        let raise = gravity_decompose::join::key_raise_of(&c.3, coll_of(k).oid);
        if raise != KeyRaise::None {
            crate::ops::shared::hash_raise::at_key(b, env, node, row, CellRef { cell: &c.0, val: c.1, null: c.2 }, &c.3, raise);
        }
        cells.push_in(c);
    }
    cells
}

/// A key's clause collation.
fn coll_of(k: &KeySrc) -> KeyColl {
    match *k {
        KeySrc::Col(_, _, c) | KeySrc::Ex(_, _, c) | KeySrc::Held(_, _, c, _) | KeySrc::NameCol(_, _, c) | KeySrc::NameEx(_, _, c) => c,
    }
}

/// The image of non-NULL key cells, in scope `Program` (both sides' images compare), each under its clause's collation; a
/// name beside a text images as its text (keys_name.rs `text_cell`).
pub(crate) fn image(lx: &mut Lx<'_, '_, '_>, node: i32, cells: &KeyVals<'_>, keys: &[KeySrc]) -> keyimg::ImageWords {
    crate::ops::hashjoin::keys_bpchar::code_untrimmed(lx, node, cells);
    let mcx = lx.mcx;
    let cells: &PgVec<'_, (Cell, Value, Option<Value>, TypeFacts)> = &cells.iter().zip(keys).map(|(c, k)| match k {
        KeySrc::NameCol(..) | KeySrc::NameEx(..) => keys_name::text_cell(lx, node, c),
        _ => tid_word(lx, c),
    }).collect_in(mcx);
    let kc: PgVec<'_, KeyCell<'_>> = cells.iter().zip(keys).map(|(c, k)| KeyCell { cell: CellRef { cell: &c.0, val: c.1, null: None }, tf: &c.3, scope: Scope::Program, coll: coll_of(k) }).collect_in(mcx);
    let img = match cells.iter().any(|c| bucket(c.3.typid).is_some()) {
        true => bucket_words(lx, node, &kc[..]),
        false => keyimg::image(lx, &kc[..]),
    };
    if !img.exact {
        crate::not_built!(node, Variant::Kind(KindId::RepVarlenaHash));
    }
    if img.words.words.len() > JOIN_KEY_IMAGE_WORDS {
        crate::refuse!(node, Reason::Shape { why: ShapeWhy("a join key image wider than 16 words") });
    }
    img
}

/// K2: `keyimg::image` where a key keys by its range bucket form (both sides' images take this road: the build's and the
/// probe's keys are one type pair): each key's image as keysem `image::bucket_image` answers it, the Fp128 under its bucket
/// form or its hash arm.
fn bucket_words(lx: &mut Lx<'_, '_, '_>, node: i32, kc: &[KeyCell<'_>]) -> keyimg::ImageWords {
    use gravity_expr::emit::hash::{emit_image, KeyPart};
    let mcx = lx.mcx;
    let mut words = mcx::reserved_in(kc.len(), mcx);
    for k in kc {
        let w = match gravity_fn::typesem::keysem::image::bucket_image(k.tf.typid, k.coll.bytes_eq) {
            Err(why) => crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) }),
            Ok(Some(form)) => crate::rep::hosted::hash::fp_words(lx, k.cell, k.tf, form),
            Ok(None) => crate::rep::hash_image(lx, k.cell, k.tf, k.scope),
        };
        let (mut a, mut n) = ([k.cell.val; 2], 0);
        for v in w.w.into_iter().flatten() {
            a[n] = v;
            n += 1;
        }
        words.push_in((a, n, w.exact));
    }
    let parts: PgVec<'_, KeyPart<'_>> = words.iter().map(|(a, n, _)| KeyPart { words: &a[..*n], null: None }).collect_in(mcx);
    let widths = words.iter().map(|w| w.1 as u8).collect(); // alloc: Program
    keyimg::ImageWords { words: emit_image(lx.b, &parts[..]), exact: words.iter().all(|w| w.2), code: None, index: None, widths }
}

/// D388: a `tid` key (FixedPtr 6) keys as one exact word, its bytes `(bi_hi, bi_lo, ip_posid)` zero-extended (`tideq`; the
/// decl's `KeyKind::I64`). A NULL key reads a zeroed slot.
fn tid_word(lx: &mut Lx<'_, '_, '_>, &(cell, val, null, tf): &(Cell, Value, Option<Value>, TypeFacts)) -> (Cell, Value, Option<Value>, TypeFacts) {
    if cell.typid != TIDOID || !matches!(cell.rep, Rep::FixedPtr { len: 6 }) {
        return (cell, val, null, tf);
    }
    let (p, _) = crate::rep::datum(lx, CellRef { cell: &cell, val, null }, &tf);
    let b = &mut *lx.b;
    let s = b.declare_stack_slot(8, 8);
    let (z, zero, s32) = (b.stack_addr(s), b.iconst(Type::I64, 0), b.iconst(Type::I64, 32));
    b.store(z, 0, zero);
    let zi = b.conv(ConvOp::PtrToInt, Type::I64, z);
    let p = null.map_or(p, |n| b.select(n, zi, p));
    let ptr = b.conv(ConvOp::IntToPtr, Type::Ptr, p);
    let (lo, hi) = (b.load_unaligned(Type::I32, ptr, 0), b.load_unaligned(Type::I16, ptr, 4));
    let (lo, hi) = (b.conv(ConvOp::Zext, Type::I64, lo), b.conv(ConvOp::Zext, Type::I64, hi));
    let hi = b.int_bin(IntBinOp::Shl, hi, s32);
    let life = CellLife { until: Until::Row, home: CellHome::RowCx };
    (Cell { rep: Rep::Word { bytes: 8 }, typid: TIDOID, num: NumTypmod::NONE, life, at: At::Value(0), null: cell.null }, b.int_bin(IntBinOp::Or, hi, lo), null, tf)
}

/// OR of the keys' NULL flags; `None` when no key can be NULL.
pub(crate) fn any_null(b: &mut FunctionBuilder<'_>, cells: &KeyVals<'_>) -> Option<Value> {
    cells.iter().filter_map(|c| c.2).reduce(|x, y| b.int_bin(IntBinOp::Or, x, y))
}

/// D1899: PostgreSQL runs a hash join's clauses again on every candidate whose hash value matches (ExecScanHashBucket,
/// ahead of the join qual), where a probe decides a hit by the key images alone. Where that run shows, a cast to a domain
/// whose CHECK makes a VOLATILE call (effects.rs `volatile_check`: the CHECK runs once per row at its side's key and once
/// more per candidate), the probe runs the clauses as its first quals on each image-equal record (probe.rs `make`), the
/// build keeps their INNER_VAR columns (build_side.rs `read_after`) and the pair does not swap.
/// K2: so does a range_eq over a range keyed by its bucket form (`bucket_clause`). D2036: so does a clause under no builtin
/// hashable equality, whose keys are its hash values (decompose join.rs `user_keys`).
pub(crate) fn confirms(gx: &crate::graph::Gx<'_, '_>, clauses: &types_nodes::list::NodeList<'_>) -> bool {
    clauses.iter().any(|c| crate::effects::volatile_check(gx, c) || bucket_clause(c) || gravity_decompose::effects::user_hash_clause(gx.facts, c))
}

/// D439: confirmed clauses (`confirms`) that call user code (a range's comparison, D2065; a domain CHECK's VOLATILE
/// call; D2036: a user hash operator, which may raise) run on each candidate a row reaches, so a pull (a cursor's FETCH,
/// Execute's max_rows) asks pipeline `k` for the rows it owes, never a block: a row past its count would run them on
/// candidates PG never reached.
pub(crate) fn pull_owed(px: &mut Px<'_, '_>, k: usize, clauses: &types_nodes::list::NodeList<'_>) {
    let gx = px.gx;
    if clauses.iter().any(|c| crate::effects::range_user_cmp(gx, c) || crate::effects::volatile_check(gx, c) || gravity_decompose::effects::user_hash_clause(gx.facts, c)) {
        px.declare_proto(gravity_program::protos::Proto::new(gravity_program::protos::ProtoKind::Volatile, k as u16));
    }
}

/// K2: key type `t`'s bucket form id: a range whose subtype opclass the exact image cannot keep (KeySem's answer, K4:
/// `keysem::hashjoin::bucket`).
pub(crate) fn bucket(t: u32) -> Option<u64> {
    gravity_fn::typesem::keysem::hashjoin::bucket(t)
}

/// K2 (D1889): hash clause `c` is the range's own `=` (typesem `bucket_eq`: range_eq) over ranges keyed by their bucket
/// form. PG buckets them by hash_range (the subtype's default hash function) and runs range_eq on each candidate, whose
/// bounds compare through the subtype opclass's FUNCTION 1 (range_cmp_bounds: a user function, VOLATILE too, twice for
/// two equal non-empty ranges).
pub(crate) fn bucket_clause(c: Node<'_>) -> bool {
    let eq = |t: u32, op: u32| gravity_fn::typesem::bucket_eq(t, op).unwrap_or(false);
    c.as_op_expr().is_some_and(|o| o.args.len() == 2 && o.args.iter().all(|a| gravity_decompose::catalog::expr_type::type_of(a).is_some_and(|t| eq(t, o.opno))))
}

/// K2: the Shape probe `j` of `p` names when a key keys by its range bucket form where the probe cannot decide its
/// candidates as PG does: its clause is not the range's own `=` (another operator's equality and hash function are its
/// own), or an expanding step (a join, a set function) or a row lock or write follows: a later step's stop, or an EPQ
/// recheck, re-enters the row's last match and would run range_eq, and the comparison it calls, again (the qual holds no
/// result of a call that is not VOLATILE).
pub(crate) fn bucket_refusal(p: &gravity_decompose::PipelineNode<'_>, j: usize, clauses: &types_nodes::list::NodeList<'_>, decls: &[JoinKeyDecl<'_>]) -> Option<&'static str> {
    let keyed = |d: &JoinKeyDecl<'_>| bucket(d.key.types.0).is_some() || bucket(d.key.types.1).is_some();
    let ok = |i: usize| clauses.iter().nth(i).is_some_and(bucket_clause);
    if decls.iter().enumerate().any(|(i, d)| keyed(d) && !ok(i)) {
        return Some("a range key whose subtype opclass is not the subtype's default under a hash clause that is not the range's own = (K2: range_eq on each candidate decides it)");
    }
    let reenters = p.ops.get(j + 1..).unwrap_or_default().iter().any(|op| matches!(op, Op::Join(_) | Op::Srf(_) | Op::Lock(_) | Op::Write(_)));
    (decls.iter().any(keyed) && reenters).then_some("a range key decided by range_eq on each candidate under a later expanding step, row lock or write (re-entering the row's last match would run the comparison again)")
}

#[cfg(test)]
mod tests {
    use gravity_decompose::catalog::typefacts::{opaque, row_of, TypeFacts};

    fn same(a: TypeFacts, b: TypeFacts) -> bool {
        gravity_decompose::join::same_rep(&a, &b)
    }

    /// V4R-75 cross-type key: a binary-coercible pair of one representation keys as one word (regproc / regclass = oid,
    /// varchar = text, a domain = its base, an enum's domain = the enum); int4 = oid (a relabel whose sign extension
    /// differs), date = timestamp (widths differ) and two unrelated types the table does not know do not.
    #[test]
    fn same_rep_accepts_one_representation_only() {
        let t = |o: u32| *row_of(o).unwrap();
        let dom = |o: u32, base: u32| TypeFacts { typid: o, ..t(base) };
        let (en, en2) = (opaque(70_001, 70_001, 4, true, true), opaque(70_003, 70_003, 4, true, true));
        let en_dom = opaque(70_002, 70_001, 4, true, false);
        for (a, b) in [(t(24), t(26)), (t(2205), t(26)), (t(2206), t(26)), (t(1043), t(25)), (dom(70_010, 23), t(23)), (en_dom, en), (en, en_dom)] {
            assert!(same(a, b), "{} = {} is one representation", a.typid, b.typid);
        }
        let user = opaque(70_004, 70_004, 4, true, false);
        for (a, b) in [(t(23), t(26)), (t(1082), t(1114)), (t(21), t(26)), (t(25), t(19)), (en, en2), (user, t(26)), (user, en), (dom(70_010, 23), t(26))] {
            assert!(!same(a, b), "{} = {} must stay a cross-type key", a.typid, b.typid);
        }
    }

    /// OIDW-33: integer_ops' types (and a domain over one) key as one integer word; a same-image type of another family
    /// (money, time) or a by-value type of another image (oid, date, float8) does not. Without the families (a capture)
    /// the images decide.
    #[test]
    fn one_integer_is_one_family_of_integer_images() {
        use gravity_decompose::catalog::{AttFact, ExprFp, Facts, IndexFact, NoFacts, RelFact};
        struct Fams;
        impl Facts for Fams {
            fn rel(&self, _: u32) -> Option<RelFact> {
                None
            }
            fn att(&self, _: u32, _: i16) -> Option<AttFact> {
                None
            }
            fn index(&self, _: u32) -> Option<IndexFact> {
                None
            }
            fn indexes_of(&self, _: u32, _: &mut dyn FnMut(IndexFact)) {}
            fn index_expr(&self, _: u32, _: u8) -> Option<ExprFp> {
                None
            }
            // stand-ins for the default btree families integer_ops, money_ops, time_ops, oid_ops, datetime_ops, float_ops
            fn default_family(&self, t: u32) -> Option<u32> {
                Some(match t {
                    20 | 21 | 23 => 1,
                    790 => 2,
                    1083 => 3,
                    26 => 4,
                    1082 | 1114 => 5,
                    701 => 6,
                    _ => return None,
                })
            }
        }
        let t = |o: u32| *row_of(o).unwrap();
        let dom = |o: u32, base: u32| TypeFacts { typid: o, ..t(base) };
        let one = |f: &dyn Facts, a: TypeFacts, b: TypeFacts| gravity_decompose::join::one_integer(f, &a, &b);
        for (a, b) in [(t(21), t(23)), (t(23), t(20)), (t(20), t(21)), (dom(70_010, 23), t(20)), (dom(70_011, 21), dom(70_012, 20))] {
            assert!(one(&Fams, a, b), "{} = {} keys one integer word", a.typid, b.typid);
        }
        for (a, b) in [(t(790), t(20)), (t(1083), t(20)), (t(26), t(23)), (t(1082), t(23)), (t(701), t(20)), (dom(70_013, 790), t(20))] {
            assert!(!one(&Fams, a, b), "{} = {} is no integer pair", a.typid, b.typid);
        }
        assert!(one(&NoFacts, t(21), t(20)) && !one(&NoFacts, t(26), t(23)) && !one(&NoFacts, t(1082), t(23)));
    }
}

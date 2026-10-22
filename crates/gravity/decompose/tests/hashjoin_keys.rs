// Key descriptors (group KeyDecl, SortKey, JoinKeyDecl) agree with the type-facts table across the plan corpus; D181's
// multi-projection key shapes and D197's numeric bound.
use std::fs;
use std::path::PathBuf;

use gravity_abi::breaker::KeyKind;
use gravity_decompose::ops::shared::sort::{KeyImage, NO_SCALE};
use gravity_decompose::join::{pair, JoinDesc, JoinDict, Side};
use gravity_decompose::ops::shared::agg_class::{Class, Ext};
use gravity_decompose::catalog::typefacts::{facts_of, key_of, unify, KeyColl, WordImage, DEFAULT_COLLATION};
use gravity_decompose::{corpus, decompose_in, NoFacts, Op, PipelineGraph};
use gravity_program::{Arena, NumTypmod};
use mcx::MemoryContext;

mod harness;

fn corpus_files() -> Vec<PathBuf> {
    let mut files = harness::fixtures_in(&harness::plans_root());
    files.extend(harness::fixtures_in(&harness::plans_root().join("regress-full")));
    files
}

fn each_graph(files: &[PathBuf], mut f: impl FnMut(&str, &gravity_decompose::corpus::FixtureFacts, &PipelineGraph<'_>)) {
    for p in files {
        let text = fs::read_to_string(p).unwrap();
        let ctx = MemoryContext::new("fixture");
        let Ok(fx) = corpus::load(ctx.mcx(), &text) else { continue };
        let arena = Arena::new(1 << 20);
        let Ok(g) = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx) else { continue };
        f(&fx.name, &fx.facts, &g);
    }
}

#[test]
fn group_keys_agree_with_their_rows() {
    let mut n = 0;
    each_graph(&corpus_files(), |name, facts, g| {
        for gd in g.groups.iter() {
            for k in gd.keys.iter() {
                n += 1;
                let Some(tf) = facts_of(facts, k.tf) else { continue };
                if tf.image == WordImage::NumMantissa {
                    if k.image == KeyImage::NumMantissa {
                        assert!(k.precision as u16 <= 18 && (0..=18).contains(&k.scale), "{name}: D197 a mantissa key fits one word");
                    }
                    continue;
                }
                assert_eq!(k.kind, tf.kind, "{name}: key over type {} takes its row's kind", k.tf);
                assert_eq!(k.words, tf.words, "{name}: key over type {}", k.tf);
                assert_eq!(k.scale, NO_SCALE, "{name}");
            }
        }
    });
    assert!(n > 100, "the corpus has group keys ({n})");
}

#[test]
fn sort_keys_agree_with_their_rows() {
    let mut n = 0;
    each_graph(&corpus_files(), |name, _, g| {
        for p in g.pipelines.iter() {
            for op in p.ops.iter() {
                let Op::Sort(s) = op else { continue };
                for k in s.keys.iter() {
                    n += 1;
                    let e = s.tlist.nth(k.resno as usize - 1).as_target_entry().unwrap().expr;
                    let Some(tf) = gravity_decompose::catalog::expr_type::type_of(e).and_then(|t| gravity_decompose::catalog::typefacts::row_of(t)) else { continue };
                    if tf.image == WordImage::NumMantissa {
                        assert!(k.image == KeyImage::NumMantissa || k.image == KeyImage::NumNormalized || k.image == KeyImage::Raw, "{name}");
                        assert_eq!(k.scale != NO_SCALE, k.image == KeyImage::NumMantissa, "{name}: a scale iff the mantissa image");
                        continue;
                    }
                    assert_eq!(k.kind, tf.kind, "{name}: sort key over type {}", tf.typid);
                    assert_eq!(k.scale, NO_SCALE, "{name}");
                }
            }
        }
    });
    assert!(n > 100, "the corpus has sort keys ({n})");
}

#[test]
fn join_key_pairs_unify_and_both_sides_carry_them() {
    let mut n = 0;
    each_graph(&corpus_files(), |name, facts, g| {
        for (i, p) in g.pipelines.iter().enumerate() {
            for op in p.ops.iter() {
                let Op::Join(JoinDesc::Probe { slot, decls, keys, .. }) = op else { continue };
                assert_eq!(decls.len(), keys.len(), "{name}: one decl per key");
                let build = g.pipelines[..i].iter().rev().find(|q| q.desc.produces() == Some(*slot));
                if let Some(bd) = build.and_then(|q| q.ops.iter().find_map(|o| if let Op::Join(JoinDesc::Build { decls, .. }) = o { Some(decls) } else { None })) {
                    assert_eq!(bd.len(), decls.len(), "{name}: the build carries the probe's decls");
                }
                for d in decls.iter().map(|d| d.key) {
                    n += 1;
                    let (Some(a), Some(b)) = (facts_of(facts, d.types.0), facts_of(facts, d.types.1)) else { continue };
                    if a.image == WordImage::NumMantissa && b.image == WordImage::NumMantissa {
                        assert!(d.kind.is_some(), "{name}: a numeric pair always keys");
                        continue;
                    }
                    assert_eq!(d.kind, unify(a.kind, b.kind), "{name}: pair {:?}", d.types);
                }
            }
        }
    });
    assert!(n > 50, "the corpus has hash join keys ({n})");
}

#[test]
fn numeric_pairs_take_the_larger_scale_when_it_fits() {
    let side = |p: u16, s: i16| Side { typid: Some(1700), num: NumTypmod::of(p, s), code: false };
    let d = pair(&NoFacts, side(15, 2), side(12, 4), 0);
    assert_eq!((d.kind, d.scale), (Some(KeyKind::I64), 4), "digits 13 + scale 4 = 17 fit");
    let d = pair(&NoFacts, side(18, 2), side(12, 4), 0);
    assert_eq!((d.kind, d.image), (Some(KeyKind::Fp128), Some(WordImage::NumNormalized)), "16 digits + scale 4 do not fit");
    let d = pair(&NoFacts, side(25, 2), side(12, 2), 0);
    assert_eq!(d.image, Some(WordImage::NumNormalized), "D197: a wide side");
    let t = |code| Side { typid: Some(25), num: NumTypmod::NONE, code };
    assert_eq!(pair(&NoFacts, t(true), t(false), 100).dict, JoinDict::Fingerprint, "a code side keys by fingerprint");
    assert_eq!(pair(&NoFacts, t(false), t(false), 100).dict, JoinDict::None);
    let i = |ty: u32| Side { typid: Some(ty), num: NumTypmod::NONE, code: false };
    assert_eq!(pair(&NoFacts, i(23), i(20), 0).kind, Some(KeyKind::I64));
    assert_eq!(pair(&NoFacts, i(20), i(23), 0).kind, Some(KeyKind::I64), "symmetric");
    assert_eq!(pair(&NoFacts, i(23), i(25), 0).kind, None);
    assert_eq!(key_of(facts_of(&NoFacts, 1700).as_ref().unwrap(), NumTypmod::of(18, 18)).0, KeyKind::I64);
}

fn keys_of(name: &str) -> Vec<(u32, KeyKind, KeyImage)> {
    let f = harness::plans_root().join(format!("{name}.plan"));
    let mut out = Vec::new();
    each_graph(&[f], |_, _, g| {
        for gd in g.groups.iter() {
            out.extend(gd.keys.iter().map(|k| (k.tf, k.kind, k.image)));
        }
    });
    out
}

/// D181: a key over a multi-projection (probe-chain) pipeline reads the child's projection entry as it is.
#[test]
fn d181_keys_across_two_probes_keep_their_types() {
    let q3 = keys_of("tpch-003");
    assert_eq!(q3.iter().map(|k| (k.0, k.1)).collect::<Vec<_>>()[1..], [(1082, KeyKind::Date), (23, KeyKind::I32)], "(date, int4) across two probes");
    let q7 = keys_of("tpch-sf10-007");
    assert_eq!((q7[0].0, q7[0].2), (1042, KeyImage::Bpchar), "(bpchar, ...) never NumNormalized");
}

/// A fixture's facts under a database whose default collation is "C" (`collate_is_c`): only `coll` differs.
struct CDefault<'f>(&'f gravity_decompose::corpus::FixtureFacts);

impl gravity_decompose::Facts for CDefault<'_> {
    fn rel(&self, relid: u32) -> Option<gravity_decompose::RelFact> {
        self.0.rel(relid)
    }
    fn att(&self, relid: u32, attno: i16) -> Option<gravity_decompose::AttFact> {
        self.0.att(relid, attno)
    }
    fn att_range(&self, relid: u32, attno: i16) -> Option<(i64, i64)> {
        self.0.att_range(relid, attno)
    }
    fn index(&self, indexid: u32) -> Option<gravity_decompose::IndexFact> {
        self.0.index(indexid)
    }
    fn index_am(&self, indexid: u32) -> gravity_decompose::IndexAm {
        self.0.index_am(indexid)
    }
    fn indexes_of(&self, relid: u32, f: &mut dyn FnMut(gravity_decompose::IndexFact)) {
        self.0.indexes_of(relid, f)
    }
    fn index_ext(&self, indexid: u32) -> gravity_decompose::catalog::IndexExt {
        self.0.index_ext(indexid)
    }
    fn att_generated(&self, relid: u32, attno: i16) -> u8 {
        self.0.att_generated(relid, attno)
    }
    fn key(&self, req: gravity_decompose::catalog::KeyReq) -> Option<gravity_decompose::catalog::KeyOrder> {
        self.0.key(req)
    }
    fn op_image_eq(&self, opno: u32) -> Option<(u32, u32)> {
        self.0.op_image_eq(opno)
    }
    fn instead_of(&self, relid: u32) -> gravity_decompose::InsteadOf {
        self.0.instead_of(relid)
    }
    fn partition_leaves(&self, root: u32, f: &mut dyn FnMut(gravity_decompose::catalog::LeafFact)) -> bool {
        self.0.partition_leaves(root, f)
    }
    fn index_expr(&self, indexid: u32, col: u8) -> Option<gravity_decompose::ExprFp> {
        self.0.index_expr(indexid, col)
    }
    fn coll(&self, oid: u32) -> KeyColl {
        if oid == DEFAULT_COLLATION { KeyColl { oid, bytes_eq: true, bytes_order: true } } else { KeyColl::of(oid) }
    }
}

/// D247: a key's collation facts are the source's (`Facts::coll`: the catalog's `collate_is_c` / deterministic), never a
/// const over the oid. The same plans (a text ORDER BY top-k and a text min under the default collation) carry `bytes_order` on
/// their sort keys and min / max declarations exactly when the source says the default orders by bytes.
#[test]
fn d247_default_collation_order_is_the_sources_fact() {
    use gravity_decompose::Facts;
    for (name, c_default) in [("bench-071", false), ("bench-071", true), ("aggs-009", false), ("aggs-009", true)] {
        let text = fs::read_to_string(harness::plans_root().join(format!("{name}.plan"))).unwrap();
        let ctx = MemoryContext::new("fixture");
        let fx = corpus::load(ctx.mcx(), &text).expect("loads");
        let arena = Arena::new(1 << 20);
        let cd = CDefault(&fx.facts);
        let facts: &dyn Facts = if c_default { &cd } else { &fx.facts };
        let g = decompose_in(arena.handle(), fx.stmt, facts, fx.mcx).expect("decomposes");
        let mut colls: Vec<KeyColl> = g.pipelines.iter().flat_map(|p| p.ops.iter()).filter_map(|op| if let Op::Sort(s) = op { Some(s.keys.iter().map(|k| k.coll)) } else { None }).flatten().collect();
        colls.extend(g.groups.iter().flat_map(|gd| gd.aggs.iter()).filter(|a| matches!(a.class, Class::Min(Ext::Text) | Class::Max(Ext::Text))).map(|a| a.coll));
        let under_default: Vec<KeyColl> = colls.into_iter().filter(|c| c.oid == DEFAULT_COLLATION).collect();
        assert!(!under_default.is_empty(), "{name}: a key under the default collation");
        assert!(under_default.iter().all(|c| c.bytes_order == c_default && c.bytes_eq), "{name} (C default {c_default}): {under_default:?}");
    }
}

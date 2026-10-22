//! Rep decided once (v3 design §3.2, kept): a cell is read, keyed, ordered, retained and blocked only through its Rep's arm,
//! and `arm_kind` (the one `match` on Rep) names the arm's kind. Every Rep × accessor call whose arm is not built yet
//! refuses `NotBuilt { hook: "rep", variant: the arm's kind, milestone: its declared one }`, named at the calling node. A retain of a
//! cell that already lives long enough asks no arm and emits nothing, and `datum` answers a second read from the row's memo.
//! The outcomes are the same with SCRIBBLE on.
use gravity_decompose::catalog::typefacts::{opaque, row_of, KeyColl, TypeFacts, WordImage};
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::Type;
use gravity_lowerv4::cell::{At, Cell, CellHome, CellLife, LaneId, Nullable};
use gravity_lowerv4::face::kinds::{Face, KindId, KINDS};
use gravity_lowerv4::rep::{self, arm_kind, Accessor, BlockRef, CellRef, Lx, RetainHome, Scope, StoredRef};
use gravity_lowerv4::{catch, ops as registry, Reason, Refusal, Variant};
use gravity_program::{DictRef, LifeId, NumTypmod, NumForm, Rep, Until};

/// Every Rep with a type it carries: int8, text, uuid, cstring, a code lane of text, numeric(15,2), and int4[] (a varlena
/// with no native image: the hosted arms).
fn reps() -> [(Rep, u32); 8] {
    [
        (Rep::Word { bytes: 8 }, 20),
        (Rep::VarlenaPtr, 25),
        (Rep::FixedPtr { len: 16 }, 2950),
        // macaddr: a fixed by-reference key with no native image orders through rep.hosted.order.fixed (M6-REPS HO)
        (Rep::FixedPtr { len: 6 }, 829),
        (Rep::CstrPtr, 2275),
        (Rep::Code { dict: DictRef::lane(3) }, 25),
        (Rep::PackedNum { facts: NumTypmod::of(15, 2), form: NumForm::Window }, 1700),
        (Rep::VarlenaPtr, 1007),
    ]
}

fn tf(typid: u32) -> TypeFacts {
    row_of(typid).copied().or_else(|| row_of(25).copied()).expect("type facts rows")
}

fn cell(rep: Rep, typid: u32, until: Until) -> Cell {
    let at = if matches!(rep, Rep::Code { .. }) { At::Lane(LaneId::col(3)) } else { At::Value(1) };
    Cell { rep, typid, num: rep::num_of(rep), life: CellLife { until, home: CellHome::RowCx }, at, null: Nullable::Maybe }
}

/// The calls one accessor makes (datum: datum, retain, block; hash: image, eq; order: image), each in a fresh function:
/// the refusal each gives (`None` = served) and the instructions it emitted.
fn run(acc: Accessor, c: Cell, retain_until: Until) -> Vec<(Option<Refusal>, usize)> {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    let calls: usize = match acc {
        Accessor::Datum => 3,
        Accessor::Hash => 2,
        Accessor::Order => 1,
    };
    (0..calls)
        .map(|call| {
            let mut pb = ProgramBuilder::new();
            let f = pb.declare_func("m", &[Type::Ptr], &[Type::I64]);
            let mut b = pb.define(f);
            let p = b.params()[0];
            let before = insts(&b);
            let t = tf(c.typid);
            let r = {
                let mut cx = Lx::new(&mut b, 11, rig.mcx());
                let cr = CellRef { cell: &c, val: p, null: None };
                catch(11, || match (acc, call) {
                    (Accessor::Datum, 0) => drop(rep::datum(&mut cx, cr, &t)),
                    (Accessor::Datum, 1) => drop(rep::retain(&mut cx, cr, &t, retain_until, RetainHome::Record(LifeId(0)))),
                    (Accessor::Datum, _) => drop(rep::block(&mut cx, cr, &t, BlockRef { elem: p, rows: p })),
                    (Accessor::Hash, 0) => drop(rep::hash_image(&mut cx, cr, &t, Scope::Program)),
                    (Accessor::Hash, _) => drop(rep::eq(&mut cx, cr, StoredRef { rep: c.rep, ptr: p, off: 0 }, &t)),
                    (Accessor::Order, _) => drop(rep::order_words(&mut cx, cr, &t, KeyColl::of(100))),
                })
            };
            (r.err(), insts(&b) - before)
        })
        .collect()
}

fn insts(b: &FunctionBuilder<'_>) -> usize {
    b.func().blocks.iter().map(|x| x.insts.len()).sum()
}

fn matrix() -> usize {
    let mut refused = 0;
    for (rep, typid) in reps() {
        for acc in Accessor::ALL {
            let k = arm_kind(rep, acc, &tf(typid));
            assert_eq!(k.decl().face, Face::Rep, "{rep:?} {} names {}, not a Rep arm", acc.name(), k.name());
            if registry::kind(k).is_some() {
                continue; // D243: a built arm serves; only the unbuilt ones must refuse by name
            }
            for (r, _) in run(acc, cell(rep, typid, Until::Row), Until::Life) {
                let r = r.unwrap_or_else(|| panic!("{rep:?} {} served at M0", acc.name()));
                assert_eq!(r.reason, Variant::Kind(k).not_built(), "{rep:?} {}", acc.name());
                assert!(matches!(r.reason, Reason::NotBuilt { hook: "rep", .. }));
                assert_eq!(r.node, 11);
                refused += 1;
            }
        }
        let out = run(Accessor::Datum, cell(rep, typid, Until::Statement), Until::Life);
        assert_eq!((out[1].0, out[1].1), (None, 0), "{rep:?}: a cell that lives past `until` is retained as is");
    }
    refused
}

#[test]
fn every_arm_refuses_by_name() {
    let n = matrix();
    gravity_fn::result_life::scribble::force(true);
    assert_eq!(matrix(), n, "the same outcomes with SCRIBBLE on");
    gravity_fn::result_life::scribble::force(false);
}

/// `arm_kind` reaches every Rep kind `face/kinds.rs` declares, and only those: no arm is decided anywhere else.
#[test]
fn arm_kind_covers_the_rep_kinds() {
    let mut seen: Vec<KindId> = reps().iter().flat_map(|&(rep, typid)| Accessor::ALL.map(|acc| arm_kind(rep, acc, &tf(typid)))).collect();
    seen.sort();
    seen.dedup();
    let declared: Vec<KindId> = KINDS.iter().filter(|d| d.face == Face::Rep).map(|d| d.id).collect();
    assert_eq!(seen, declared);
    assert_eq!(arm_kind(Rep::VarlenaPtr, Accessor::Hash, &tf(1007)), KindId::RepHostedHash, "int4[] keys through the hosted arm");
    assert_eq!(arm_kind(Rep::CstrPtr, Accessor::Datum, &tf(2275)), KindId::RepVarlenaDatum, "a cstring's datum is its pointer");
}

#[test]
fn datum_memo_answers_the_second_read() {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("m", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let p = b.params()[0];
    let c = cell(Rep::VarlenaPtr, 25, Until::Row);
    let other = b.iconst(Type::I64, 15);
    let mut cx = Lx::new(&mut b, 2, rig.mcx());
    let at_p = CellRef { cell: &c, val: p, null: None };
    cx.memo.put(at_p, (p, p));
    assert_eq!(catch(2, || rep::datum(&mut cx, at_p, &tf(25))).expect("memoised"), (p, p));
    assert!(cx.memo.get(CellRef { cell: &c, val: other, null: None }).is_none(), "another backing value is not answered");
    cx.memo.clear();
    assert!(cx.memo.is_empty());
}

/// OIDW-13: a key orders by its payload bytes when its type's default btree comparator is a bytes compare (KeySem's
/// `orders_by_bytes` over the type row, read through `rep::bytes_order`), never by a type list: bytea, name, text,
/// bpchar, varchar do; inet and cidr (network_cmp), numeric and the word types do not; a domain carries its base's
/// comparator (D2347).
#[test]
fn bytes_order_is_the_comparators() {
    for typid in [17, 19, 25, 1042, 1043] {
        assert!(rep::bytes_order(row_of(typid).unwrap()), "{typid}: a bytes comparator");
    }
    for typid in [650, 869, 1700, 23, 2950] {
        assert!(!rep::bytes_order(row_of(typid).unwrap()), "{typid}: not a bytes order");
    }
    let net = row_of(869).unwrap();
    assert!(matches!(net.image, WordImage::Bytes { .. }) && net.image.equality(), "inet still hashes and compares equal by its bytes");
    assert!(!rep::bytes_order(&TypeFacts { typid: 90001, ..*net }), "a domain over inet orders as inet");
    assert!(rep::bytes_order(&TypeFacts { typid: 90002, ..*row_of(25).unwrap() }), "a domain over text orders as text");
    assert!(!rep::bytes_order(&opaque(90003, 90003, -1, false, false)), "a type the table does not list has no bytes order");
}

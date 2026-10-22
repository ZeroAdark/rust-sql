//! The seam text is interned from compile-time facts only (main's ruling, 09-23, kept): refusing 1,000 times per reason
//! with a different field value each time grows the intern set by at most one entry per static text (each declared kind
//! and road for `NotBuilt`, one per other reason here), and a second identical pass grows it by none.
use gravity_lowerv4::face::kinds::KINDS;
use gravity_lowerv4::face::strategy::STRATEGIES;
use gravity_lowerv4::admit::refusal::{interned, CollWhy, NumOp, ShapeWhy};
use gravity_lowerv4::{Reason, Refusal, Variant};
use gravity_program::{LifeId, NumTypmod};

fn pass() {
    let variants: Vec<Variant> = KINDS.iter().map(|d| Variant::Kind(d.id)).chain(STRATEGIES.iter().map(|&d| Variant::Strategy(d))).collect();
    for i in 0..1000u32 {
        let reasons = [
            variants[i as usize % variants.len()].not_built(),
            Reason::NumericTooWide { precision: (i % 1000) as u16, op: NumOp::Sum },
            Reason::NoOrder { typid: 16384 + i },
            Reason::NoHash { typid: 16384 + i },
            Reason::Collation { coll: 100 + i, why: CollWhy::NonDeterministic },
            Reason::JoinScale { a: NumTypmod::of(10, (i % 10) as i16), b: NumTypmod::of(12, 2) },
            Reason::Shape { why: ShapeWhy("a volatile call an OFFSET drops") },
            Reason::Width { needed: i, have: 8 },
            Reason::Grant { life: LifeId(i as u16), needed: 1 << 20 | i as u64, share: 4096 },
            Reason::Stopgap { d: i as u16 },
            Reason::UnsupportedAm { relid: 16384 + i },
            Reason::Bug { payload: "checked at compile" },
            Reason::Interrupted,
        ];
        assert_eq!(reasons.len(), Reason::VARIANTS, "one of each reason");
        for r in reasons {
            let _ = Refusal { reason: r, node: i as i32, native_rel: 0 }.to_seam();
        }
    }
}

#[test]
fn intern_set_bounded_by_static_texts() {
    let before = interned();
    pass();
    let grown = interned() - before;
    let bound = KINDS.len() + STRATEGIES.len() + Reason::VARIANTS - 1;
    assert!(grown <= bound, "{grown} interned texts for a bound of {bound}");
    let before = interned();
    pass();
    assert_eq!(interned(), before, "a second pass interns nothing new");
}

// The census reads a function's facts two ways: decompose asks the plan fixture's Facts (`corpus::FixtureFacts`, whose
// `func` is the builtin table `effects::procfacts`), the lowering asks the rig's Catalog (`common::RigCatalog`, read from
// pg_proc.dat). Both must answer every function alike, so a reader that moves from one to the other moves no census
// answer: each pg_proc.dat row the same provolatile / proisstrict / proleakproof / proparallel, and a function past
// pg_proc.dat (an initdb SQL function, a user function) unknown to both unless the fixture's `-- func` line names it,
// and then both read that line.
use gravity_lowerv4_it as common;

use gravity_decompose::corpus::FixtureFacts;
use gravity_decompose::effects::Parallel;
use gravity_decompose::Facts;
use gravity_expr::Catalog;

#[test]
fn census_facts_and_catalog_answer_every_function_alike() {
    let cat: &dyn Catalog<'static> = common::RigCatalog::get();
    let facts = FixtureFacts::default();
    let (mut both, mut bad) = (0usize, Vec::new());
    for oid in 1..=u32::from(u16::MAX) {
        match (cat.func(oid).ok(), facts.func(oid)) {
            (None, None) => {}
            (Some(c), Some(f)) => {
                both += 1;
                let parallel = match f.parallel {
                    Parallel::Safe => b's',
                    Parallel::Restricted => b'r',
                    Parallel::Unsafe => b'u',
                };
                let want = (Some(f.volatility.letter() as u8), f.strict, Some(f.leakproof), Some(parallel));
                if (c.volatility, c.strict, c.leakproof, c.parallel) != want {
                    bad.push(format!("{oid}: catalog {:?}, facts {f:?}", (c.volatility, c.strict, c.leakproof, c.parallel)));
                }
            }
            (c, f) => bad.push(format!("{oid}: catalog {}, facts {}", if c.is_some() { "a row" } else { "none" }, if f.is_some() { "a row" } else { "none" })),
        }
    }
    assert!(bad.is_empty(), "{} functions the census reads two ways:\n{}", bad.len(), bad.join("\n"));
    assert!(both > 3000, "only {both} functions answered by both: the pg_proc.dat reader or the builtin table is empty");
}

#[test]
fn a_fixtures_func_line_answers_both_readers() {
    let mut facts = FixtureFacts::default();
    assert_eq!(facts.line("func 16432 vol=s strict=f leakproof=f parallel=u retset=f nargs=1 lang=14 nodearg=f"), Ok(true));
    assert_eq!(facts.line("func 16404 vol=v strict=f leakproof=f parallel=u retset=t nargs=0 lang=14 nodearg=f"), Ok(true));
    let cat = common::FixtureCatalog::of(&facts);
    let cat: &dyn Catalog<'static> = &cat;
    for oid in [16432, 16404] {
        let (c, f) = (cat.func(oid).expect("the line's row"), facts.func(oid).expect("the line's props"));
        assert_eq!((c.volatility, c.strict, c.leakproof), (Some(f.volatility.letter() as u8), f.strict, Some(f.leakproof)), "{oid}");
    }
    let c = cat.func(16404).unwrap();
    assert_eq!((c.retset, c.nargs, c.prolang, c.parallel), (true, 0, Some(14), Some(b'u')));
    // a builtin and an OID no line names answer as before
    assert_eq!(cat.func(1066).ok().map(|c| c.volatility), Some(Some(b'i')));
    assert!(cat.func(16500).is_err() && facts.func(16500).is_none());
    let err = facts.line("func 16501 vol=x strict=f leakproof=f parallel=u retset=f nargs=0 lang=14 nodearg=f").unwrap_err();
    assert!(err.contains("bad func vol="), "{err}");
}

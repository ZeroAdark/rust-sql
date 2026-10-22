//! RD-prefetch.fp (the structural-overfit catalog: a road no regress statement is served by). Road `prefetch.fp`'s premise
//! is a group key served as a dictionary code (`Applies::CodeKeyed`: kind rep.code.hash), and that kind is dict's one code
//! verdict (lowerv4 dict/premise.rs `code_expr`: the key reads exactly one code lane, only as a value), not a statement's
//! shape. The does-not-apply test: every census plan fixture whose graph keys a group by a code takes the road and no
//! other fixture does, the fixtures of the corpora that are no benchmark among them (aggs-039 / aggs-041 today, of 28); the
//! same fixture with its relations' access method read as heap (no code lane) does not.
use gravity_lowerv4_it as common;

use std::collections::BTreeSet;

use gravity_lowerv4::applies::{self, Prop, PropSet};
use gravity_lowerv4::face::kinds::{KindId, KindSet};
use gravity_lowerv4::face::strategy::PREFETCH_FP;

/// (the graph's kinds include rep.code.hash, the roads it takes include prefetch.fp), `None` when decompose refuses it.
fn code_keyed_and_takes(text: &str) -> Option<(bool, bool)> {
    let (kinds, props) = common::props_fixture(text)?;
    let (mut k, mut p) = (KindSet::EMPTY, PropSet::default());
    kinds.iter().for_each(|n| k.add(KindId::from_name(n).unwrap_or_else(|| panic!("unknown kind {n}"))));
    props.iter().for_each(|n| p.add(Prop::from_name(n).unwrap_or_else(|| panic!("unknown prop {n}"))));
    Some((k.has(KindId::RepCodeHash), applies::strategies_for(k, p).has(PREFETCH_FP)))
}

/// The targeted corpora (the catalog's bench side: fieldobs, TPC-H, the bench corpus) by fixture name.
fn bench(name: &str) -> bool {
    ["fieldobs-", "tpch-", "bench-"].iter().any(|p| name.starts_with(p))
}

#[test]
fn prefetch_fp_takes_every_code_keyed_group_and_no_other() {
    let (mut taken, mut nonbench, mut heap_twins, mut bad) = (0, BTreeSet::new(), 0, Vec::new());
    for (name, text) in common::files(&common::plans_dir(), "plan") {
        let Some((code, got)) = code_keyed_and_takes(&text) else { continue };
        if got != code {
            bad.push(format!("{name}: code-keyed {code}, prefetch.fp {got}"));
            continue;
        }
        if !got {
            continue;
        }
        taken += 1;
        if !bench(&name) {
            nonbench.insert(name.clone());
        }
        // the near form: the same plan over heap relations (the header's access method, nothing else moved): no code lane
        let heap: String = text.lines().map(|l| if l.starts_with("-- rel ") { l.replace(" am=pgrcolumnar", " am=heap") } else { l.to_owned() } + "\n").collect();
        match code_keyed_and_takes(&heap) {
            Some((false, false)) => heap_twins += 1,
            Some(x) => bad.push(format!("{name}: its heap twin is (code-keyed, prefetch.fp) {x:?}")),
            None => {}
        }
    }
    eprintln!("prefetch.fp: {taken} code-keyed fixtures take it ({} outside the benchmarks: {nonbench:?}); {heap_twins} heap twins do not", nonbench.len());
    assert!(bad.is_empty(), "{}", bad.join("\n"));
    assert!(nonbench.iter().any(|n| n.starts_with("aggs-")), "no aggs- fixture took prefetch.fp: {nonbench:?}");
    assert!(heap_twins >= nonbench.len(), "{heap_twins} heap twins checked for {} non-benchmark fixtures", nonbench.len());
}

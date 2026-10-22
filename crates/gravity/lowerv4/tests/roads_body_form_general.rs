//! RD-body.form, kept (the structural-overfit catalog: a road no regress statement is served by). Road `body.form`'s
//! premise is a pgrc scan (`Applies::Pgrc`: kind src.pgrc, read off the scanned relation's access method), not a
//! statement's shape. The does-not-apply test: every census plan fixture that scans a pgrcolumnar relation takes the road,
//! the fixtures of the corpora that are no benchmark (aggs, constkey, facts) among them; the same fixture with its
//! relations' access method read as heap does not, and no heap fixture does.
use gravity_lowerv4_it as common;

use std::collections::BTreeSet;

use gravity_lowerv4::applies::{self, Prop, PropSet};
use gravity_lowerv4::face::kinds::{KindId, KindSet};
use gravity_lowerv4::face::strategy::BODY_FORM;

/// The roads `text`'s graph takes (kinds and properties as admission reads them), `None` when decompose refuses it.
fn takes_body_form(text: &str) -> Option<bool> {
    let (kinds, props) = common::props_fixture(text)?;
    let (mut k, mut p) = (KindSet::EMPTY, PropSet::default());
    kinds.iter().for_each(|n| k.add(KindId::from_name(n).unwrap_or_else(|| panic!("unknown kind {n}"))));
    props.iter().for_each(|n| p.add(Prop::from_name(n).unwrap_or_else(|| panic!("unknown prop {n}"))));
    Some(applies::strategies_for(k, p).has(BODY_FORM))
}

/// The targeted corpora (the catalog's bench side: fieldobs, TPC-H, the bench corpus) by fixture name.
fn bench(name: &str) -> bool {
    ["fieldobs-", "tpch-", "bench-"].iter().any(|p| name.starts_with(p))
}

#[test]
fn body_form_takes_every_pgrc_scan_and_no_heap_one() {
    let (mut taken, mut nonbench, mut heap_twins, mut bad) = (0, BTreeSet::new(), 0, Vec::new());
    for (name, text) in common::files(&common::plans_dir(), "plan") {
        let pgrc = text.lines().take_while(|l| l.starts_with("--")).any(|l| l.starts_with("-- rel ") && l.contains(" am=pgrcolumnar "));
        let Some(got) = takes_body_form(&text) else { continue };
        if got != pgrc {
            bad.push(format!("{name}: pgrc scan {pgrc}, body.form {got}"));
            continue;
        }
        if !pgrc {
            continue;
        }
        taken += 1;
        if !bench(&name) {
            nonbench.insert(name.clone());
        }
        // the near form: the same plan over heap relations (the header's access method, nothing else moved)
        let heap: String = text.lines().map(|l| if l.starts_with("-- rel ") { l.replace(" am=pgrcolumnar", " am=heap") } else { l.to_owned() } + "\n").collect();
        match takes_body_form(&heap) {
            Some(false) => heap_twins += 1,
            Some(true) => bad.push(format!("{name}: its heap twin takes body.form")),
            None => {}
        }
    }
    eprintln!("body.form: {taken} pgrc fixtures take it ({} outside the benchmarks: {nonbench:?}); {heap_twins} heap twins do not", nonbench.len());
    assert!(bad.is_empty(), "{}", bad.join("\n"));
    for c in ["aggs-", "constkey-", "facts-"] {
        assert!(nonbench.iter().any(|n| n.starts_with(c)), "no {c} fixture took body.form: {nonbench:?}");
    }
    assert!(heap_twins >= nonbench.len(), "{heap_twins} heap twins checked for {} non-benchmark fixtures", nonbench.len());
}

//! M1-JOIN's crate tests (main 09-24): no v4-lowered pipeline carries decompose's legacy kernel `WordsDesc`. decompose
//! numbers those words (agg.rs `W_*`, join.rs `bits_word`, setops.rs) in the graph's word block. Under v4 the words are
//! v4's own word plan, and v4's facts are protocols, so a kept `WordsDesc` names a literal's slot. The runner then raises
//! "a kernel word over a key param" (TH14, 09-24; KEYED's hash ends before it, f412670ace1). Every plan fixture that
//! lowers is checked as served and with table.sized off (a hash join lowers only then until TABLES registers H13b).
//! V4R-194: a `WordsDesc` every slot of which is a word of a Facts protocol the same pipeline declares (keyed/pairs.rs's
//! dense key: DenseLo / DenseHi) names v4's own words, not decompose's, and is allowed.
use gravity_lowerv4_it as common;

use gravity_lowerv4::face::strategy::{self, StrategySet};
use gravity_program::protos::ProtoKind;
use gravity_program::WordsDesc;

#[test]
fn no_v4_pipeline_carries_a_legacy_words_desc() {
    let (mut lowered, mut bad) = (0, Vec::new());
    for off in ["", "table.sized"] {
        strategy::set_off(StrategySet::parse(off).unwrap_or_else(|e| panic!("road_off {off:?}: {e}")));
        for (name, text) in common::files(&common::plans_dir(), "plan") {
            let Some(Ok(l)) = common::lower_fixture(&text) else { continue };
            lowered += 1;
            for (i, p) in l.descs.pipelines.iter().enumerate() {
                let facts = |s: u16| l.descs.v4.protos.iter().any(|q| q.kind == ProtoKind::Facts && q.pipeline as usize == i && (q.words.base..q.words.base + q.words.n).contains(&s));
                if let Some(w) = p.words.filter(|w| !w.slots().into_iter().filter(|&s| s != WordsDesc::NONE).all(facts)) {
                    bad.push(format!("{name} (road_off {off:?}): pipeline {i} ending {:?} keeps {w:?}", p.ends_in));
                }
            }
        }
    }
    strategy::set_off(StrategySet::EMPTY);
    assert!(lowered > 0, "no plan fixture lowered");
    assert!(bad.is_empty(), "{} v4 pipelines keep decompose's kernel words (their end's pipeline_desc clears them):\n{}", bad.len(), bad.join("\n"));
}

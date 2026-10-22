//! The live-vs-fixture facts check (decompose.md §4.8, scripts/gravity-facts-check.sh): for every fixture in
//! <plans dir> with a sidecar <live dir>/<name>.live (the server's `gravity facts:` lines for the same
//! statement, `facts::render` over `RelcacheFacts`), the fixture loader's facts rendered the same way must be
//! byte-equal per relation. Prints `checked=<relations> fixtures=<n> missing=<m>` and every mismatch; exit 1
//! on any mismatch, 2 when nothing was checked.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use gravity_decompose::catalog::render;
use gravity_decompose::Facts;
use gravity_decompose::corpus;
use mcx::MemoryContext;

fn blocks(text: &str) -> BTreeMap<u32, String> {
    let mut out = BTreeMap::new();
    let mut cur: Option<u32> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("rel ") {
            cur = rest.split_whitespace().next().and_then(|r| r.parse().ok());
            if let Some(r) = cur {
                out.insert(r, String::new()); // a relation named twice in one statement: the last block
            }
        }
        if let Some(r) = cur {
            let b: &mut String = out.entry(r).or_default();
            b.push_str(line);
            b.push('\n');
        }
    }
    out
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (plans, live) = (Path::new(&args[1]), Path::new(&args[2]));
    let (mut fixtures, mut checked, mut missing, mut bad) = (0usize, 0usize, 0usize, 0usize);
    let mut names: Vec<_> = fs::read_dir(plans).unwrap().filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "plan")).collect();
    names.sort();
    for path in names {
        let stem = path.file_stem().unwrap().to_string_lossy().to_string();
        let Ok(live_text) = fs::read_to_string(live.join(format!("{stem}.live"))) else {
            missing += 1;
            continue;
        };
        fixtures += 1;
        let text = fs::read_to_string(&path).unwrap();
        let ctx = MemoryContext::new("fixture");
        let fx = corpus::load(ctx.mcx(), &text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let live_blocks = blocks(&live_text);
        for (r, _) in fx.facts.rels.iter() {
            let mut mine = String::new();
            render(&fx.facts as &dyn Facts, r.relid, &mut mine).unwrap();
            checked += 1;
            match live_blocks.get(&r.relid) {
                Some(l) if *l == mine => {}
                Some(l) => {
                    bad += 1;
                    println!("MISMATCH {stem} rel {}:\n--- fixture\n{mine}--- live\n{l}", r.relid);
                }
                None => {
                    bad += 1;
                    println!("MISMATCH {stem} rel {}: the server logged no facts for it", r.relid);
                }
            }
        }
    }
    println!("checked={checked} fixtures={fixtures} missing={missing} mismatches={bad}");
    std::process::exit(if bad > 0 { 1 } else if checked == 0 { 2 } else { 0 });
}

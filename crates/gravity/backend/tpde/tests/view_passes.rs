//! Debug aid: `GRAVITY_VIEW_TIMING=<file with a (program ...) text>`
//! builds the view of every function `GRAVITY_VIEW_ROUNDS` times (default
//! 200, the tables reused as `Scratch` reuses them) and prints the median
//! nanoseconds per pass (`FnView::pass_ns`, `PASS_NAMES`) and the total.
//! Ignored unless the variable is set. AArch64 (the A64 immediate rules).
#![cfg(target_arch = "aarch64")]

use gravity_ir::text::parse_program;
use gravity_tpde::view::{FnView, PASS_NAMES};

fn clock() -> u64 {
    use std::sync::OnceLock;
    static T0: OnceLock<std::time::Instant> = OnceLock::new();
    T0.get_or_init(std::time::Instant::now).elapsed().as_nanos() as u64
}

#[test]
fn print_view_pass_timing() {
    let Ok(path) = std::env::var("GRAVITY_VIEW_TIMING") else { return };
    let rounds: usize = std::env::var("GRAVITY_VIEW_ROUNDS").ok().and_then(|s| s.parse().ok()).unwrap_or(200);
    let text = std::fs::read_to_string(&path).unwrap();
    let p = parse_program(&text, &|_| Some(0x1000)).unwrap_or_else(|e| panic!("{e}"));
    gravity_tpde::set_clock(clock);
    for f in &p.funcs {
        let mut samples: Vec<[u64; 19]> = Vec::with_capacity(rounds);
        let ctx = mcx::MemoryContext::new("view-passes");
        let mut view = FnView::new_in(ctx.mcx());
        for _ in 0..rounds {
            view.rebuild(f, &gravity_tpde::frontend::A64_IMM);
            samples.push(view.pass_ns);
        }
        let mut total = 0u64;
        eprintln!("fn {} ({} blocks, {} insts): median ns per pass over {rounds} rounds", f.name, f.blocks.len(), f.blocks.iter().map(|b| b.insts.len()).sum::<usize>());
        for (i, name) in PASS_NAMES.iter().enumerate() {
            let mut v: Vec<u64> = samples.iter().map(|s| s[i]).collect();
            v.sort_unstable();
            let med = v[v.len() / 2];
            total += med;
            eprintln!("  {name:18} {med:8}");
        }
        let mut sums: Vec<u64> = samples.iter().map(|s| s.iter().sum()).collect();
        sums.sort_unstable();
        eprintln!("  {:18} {total:8}   (median of the per-build sums {})", "TOTAL", sums[sums.len() / 2]);
    }
}

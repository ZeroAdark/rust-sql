//! Debug aid: `GRAVITY_VIEW_DUMP=<file with a (program ...) text>` prints
//! the view's fixed-register nominations and weights per function.
//! Ignored unless the variable is set. AArch64 only (it names the A64
//! immediate rules).
#![cfg(target_arch = "aarch64")]

use gravity_ir::text::parse_program;
use gravity_tpde::view::FnView;

#[test]
fn print_view_nominations() {
    let Ok(path) = std::env::var("GRAVITY_VIEW_DUMP") else { return };
    let text = std::fs::read_to_string(&path).unwrap();
    // The dump file may carry the disassembly after the program text: cut
    // at the first line that does not belong to the s-expression.
    let mut depth = 0i32;
    let mut end = text.len();
    let mut started = false;
    for (i, c) in text.char_indices() {
        match c {
            '(' => {
                depth += 1;
                started = true;
            }
            ')' => {
                depth -= 1;
                if started && depth == 0 {
                    end = i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    let p = parse_program(&text[..end], &|_| Some(0)).unwrap_or_else(|e| panic!("{e}"));
    let ctx = mcx::MemoryContext::new("view-debug");
    for f in &p.funcs {
        let view = FnView::build(ctx.mcx(), f, &gravity_tpde::frontend::A64_IMM);
        eprintln!("fn {}: {} values, {} blocks, has_calls {}", f.name, f.values.len(), view.blocks.len(), view.has_calls);
        let forced: Vec<u32> = (0..f.values.len() as u32).filter(|v| view.force_fixed[*v as usize]).collect();
        eprintln!("  force_fixed: {forced:?}");
        for &v in &forced {
            for (bi, b) in f.blocks.iter().enumerate() {
                for inst in &b.insts {
                    if inst.result == Some(gravity_ir::Value(v)) {
                        eprintln!("    v{v}: block {bi} {:?}", inst.op);
                    }
                }
            }
        }
        let mut weights: Vec<(u32, u32)> = (0..f.values.len() as u32).map(|v| (view.weight[v as usize], v)).collect();
        weights.sort_unstable_by(|a, b| b.cmp(a));
        eprintln!("  heaviest: {:?}", &weights[..weights.len().min(16)]);
        let renamed: Vec<(u32, u32)> = (0..f.values.len() as u32).filter(|v| view.rename[*v as usize] != *v).map(|v| (v, view.rename[v as usize])).collect();
        eprintln!("  renamed: {renamed:?}");
        let no_fixed: Vec<u32> = (0..f.values.len() as u32).filter(|v| view.no_fixed.get(*v as usize).copied().unwrap_or(false)).collect();
        eprintln!("  no_fixed: {no_fixed:?}");
        for (bi, b) in f.blocks.iter().enumerate() {
            if !b.params.is_empty() {
                eprintln!("  block {bi}: params {:?} phis {:?}", b.params.iter().map(|v| v.0).collect::<Vec<_>>(), view.blocks[bi].phis.iter().map(|v| v.0).collect::<Vec<_>>());
            }
        }
    }
}

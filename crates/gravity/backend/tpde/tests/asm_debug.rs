//! Debug aid: `GRAVITY_ASM_DUMP=<file with a (program ...) text>` compiles
//! the program and prints the AArch64 disassembly of function
//! `GRAVITY_ASM_FN` (default `pipeline`, else the last function) in the
//! server dump's format (`PGRUST_GRAVITY_DUMP`: `offset: text`), so a
//! remote dump's code can be diffed against a local build of the compiler
//! (debug vs release, run to run). Ignored unless the variable is set.
//! `GRAVITY_ASM_REGALLOC=1` prints the regalloc report first.
#![cfg(target_arch = "aarch64")]

use gravity_ir::text::parse_program;
use mcx::MemoryContext;

#[test]
fn print_entry_disassembly() {
    let Ok(path) = std::env::var("GRAVITY_ASM_DUMP") else { return };
    let text = std::fs::read_to_string(&path).unwrap();
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
    let p = parse_program(&text[..end], &|_| Some(0x1000)).unwrap_or_else(|e| panic!("{e}"));
    let want = std::env::var("GRAVITY_ASM_FN").unwrap_or_else(|_| "pipeline".into());
    let fi = p.funcs.iter().position(|f| f.name == *want).unwrap_or(p.funcs.len() - 1);
    let entry = gravity_ir::Func(fi as u32);
    let ctx = MemoryContext::new("asm_debug");
    let c = gravity_tpde::compile_recording(ctx.mcx(), &p).unwrap();
    if std::env::var_os("GRAVITY_ASM_REGALLOC").is_some() {
        // The regalloc events per block (`Compiled::regalloc_report`): where
        // the spills, reloads and phi moves of this allocation sit.
        eprintln!("--- regalloc {} ---\n{}", p.funcs[fi].name, c.regalloc_report(entry));
    }
    let range = c.mapper().get_mapped_range().unwrap();
    // SAFETY: the mapping is live for the compiled program's lifetime.
    let bytes = unsafe { range.bytes() };
    let off = c.entry(entry) - range.address();
    let mut out = String::new();
    let mut i = off;
    let mut zeros = 0;
    let mut n = 0;
    while i + 4 <= bytes.len() && n < 8000 {
        let w = u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
        if w == 0 {
            zeros += 1;
            if zeros > 16 {
                break;
            }
            i += 4;
            continue;
        }
        zeros = 0;
        let d = tpde_disarm::decode(w);
        let mut s = [0u8; 128];
        let t = tpde_disarm::da64_format(&d, &mut s).map(|k| String::from_utf8_lossy(&s[..k]).into_owned()).unwrap_or("??".into());
        out.push_str(&format!("{:5}: {t}\n", i - off));
        i += 4;
        n += 1;
    }
    eprintln!("--- asm_debug {} ({} bytes mapped) ---\n{out}--- end ---", p.funcs[fi].name, bytes.len());
}

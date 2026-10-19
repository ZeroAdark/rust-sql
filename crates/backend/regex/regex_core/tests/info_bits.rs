//! regc_lex.c `\\a` / `\\e` go through chrnamed() -> element(), which NOTEs
//! REG_ULOCALE for every multi-chr name; regex_t.re_info carries the bit.

use ::mcx::MemoryContext;
use regex_core::regex_compile::pg_regcomp;
use regex_core::regex_consts::{REG_ADVANCED, REG_ULOCALE, REG_UNONPOSIX, REG_UUNPORT};

fn info(pat: &str) -> i64 {
    let w: Vec<u32> = pat.chars().map(|c| c as u32).collect();
    let ctx = MemoryContext::new("t");
    pg_regcomp(ctx.mcx(), &w, REG_ADVANCED, 0).expect("compiles").re_info
}

#[test]
fn named_chr_escapes_note_ulocale() {
    assert_eq!(info("\\a"), (REG_UNONPOSIX | REG_ULOCALE) as i64);
    assert_eq!(info("\\e"), (REG_UNONPOSIX | REG_UUNPORT | REG_ULOCALE) as i64);
    assert_eq!(info("a") & REG_ULOCALE as i64, 0);
}

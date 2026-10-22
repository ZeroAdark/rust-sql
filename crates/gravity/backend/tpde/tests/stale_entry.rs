//! D117: a `cbz` / `cbnz` operand's reference was held across `generate_cond_branch`, so `release_spilled_regs` skipped its
//! locked register and the block laid out next — not the branch's only-entry successor — started out believing the value
//! still sat in it. In the seeded drain fold (L3b (2), bench-035) a constant 0 materialized into x2 for a `cbz` in one arm
//! was read as the loop-entry phi source on the other arm, where x2 held the resume word: a garbage group
//! [RESUME_OPEN, RESUME_OPEN] in the drain's table. The fixture is that program as the lowerer dumped it (its functions
//! renamed apart). The allocator's verifier (`RegallocEvent::StaleEntry`: a block entered other than as the only-entry
//! successor of the block before it begins with a non-fixed register assumed live) must stay silent.
//! `GRAVITY_VERIFY_IR=<ir.txt>` (a `DUMP_ALL_IR` dump) runs the verifier over every program of the corpus.
#![cfg(target_arch = "aarch64")]

use gravity_ir::text::parse_program;
use gravity_ir::*;
use mcx::MemoryContext;
use tpde_compiler::RegallocEvent;

fn stale(text: &str) -> usize {
    let p: Program = parse_program(text, &|_| Some(0x1000)).unwrap_or_else(|e| panic!("{e}"));
    let ctx = MemoryContext::new("gravity-tpde-test");
    let c = gravity_tpde::compile_recording(ctx.mcx(), &p).unwrap_or_else(|e| panic!("compile: {e:?}"));
    c.regalloc().iter().filter(|e| e.event == RegallocEvent::StaleEntry).count()
}

#[test]
fn a_jump_operand_register_does_not_outlive_its_branch() {
    assert_eq!(stale(include_str!("fixtures/drain_fold_seeded.txt")), 0);
}

/// The programs of a `DUMP_ALL_IR` dump: each `(program …)` text, its functions renamed apart (a statement's programs
/// reuse pipeline names).
fn programs(dump: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut name = String::new();
    let mut rest = dump;
    while let Some(i) = rest.find("\n(program") {
        if let Some(h) = rest[..i].rfind("=== ") {
            name = rest[h + 4..].lines().next().unwrap_or("").to_string();
        }
        let body = &rest[i + 1..];
        let (mut depth, mut end) = (0i32, body.len());
        for (k, ch) in body.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = k + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        let mut text = String::with_capacity(end);
        let mut n = 0;
        for piece in body[..end].split("(fn ") {
            if n > 0 {
                let sp = piece.find(' ').unwrap_or(0);
                text.push_str(&format!("(fn {}_{n}{}", &piece[..sp], &piece[sp..]));
            } else {
                text.push_str(piece);
            }
            n += 1;
        }
        out.push((name.clone(), text));
        rest = &body[end..];
    }
    out
}

#[test]
fn the_corpus_enters_no_block_with_a_stale_register() {
    let Ok(path) = std::env::var("GRAVITY_VERIFY_IR") else { return };
    let dump = std::fs::read_to_string(&path).unwrap();
    let (mut total, mut bad) = (0, Vec::new());
    let mut unparsed = 0;
    for (name, text) in programs(&dump) {
        // a dumped program that does not parse back (a value printed before its definition) is not this check's
        if parse_program(&text, &|_| Some(0x1000)).is_err() {
            unparsed += 1;
            continue;
        }
        total += 1;
        let n = std::panic::catch_unwind(|| stale(&text)).unwrap_or(usize::MAX);
        if n != 0 {
            bad.push((name, n));
        }
    }
    eprintln!("stale_entry: {} programs ({} unparsed), {} with a stale entry", total, unparsed, bad.len());
    for (name, n) in &bad {
        eprintln!("  {name}: {}", if *n == usize::MAX { "PANIC".to_string() } else { n.to_string() });
    }
    assert!(bad.iter().all(|&(_, n)| n == 0), "{} programs enter a block with a stale register", bad.len());
}

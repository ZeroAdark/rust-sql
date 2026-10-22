use std::fs;

use gravity_lint::asm_loop::*;

#[test]
fn classifier_reads_both_syntaxes() {
    assert_eq!(classify("ldr", "w9, [x8], #4").0, Class::Load);
    assert_eq!(classify("ldr", "x9, [x8, x10, lsl #3]").0, Class::Gather);
    assert_eq!(classify("ldrb", "w9, [x8, w10, uxtw]").0, Class::Gather);
    assert_eq!(classify("ldr", "x9, [sp, #16]").0, Class::StackLoad);
    assert_eq!(classify("str", "x9, [x29, #-24]").0, Class::StackStore);
    assert_eq!(classify("ldr", "w9, [sp, x8, lsl #2]").0, Class::StackIndexed);
    assert_eq!(classify("stp", "x9, x10, [x8]").0, Class::Store);
    assert_eq!(classify("tst", "w8, #0x1").0, Class::Test);
    assert_eq!(classify("orr", "x8, x8, x9").0, Class::Or);
    assert_eq!(classify("b.ne", "LBB3_4"), (Class::Branch, Some("LBB3_4".into())));
    assert_eq!(classify("tbz", "w8, #3, LBB3_9"), (Class::Branch, Some("LBB3_9".into())));
    assert_eq!(classify("bl", "__ZN11gravity_abi7program10arena_grow17h1E").0, Class::ColdCall);
    assert_eq!(classify("bl", "__ZN11gravity_abi7breaker7HashAgg14find_or_insert17h1E").0, Class::Call);
    assert_eq!(classify("blr", "x8").0, Class::IndirectCall);
    assert_eq!(classify("movl", "(%rax,%rcx,4), %edx").0, Class::Gather);
    assert_eq!(classify("movq", "8(%rax), %rdx").0, Class::Load);
    assert_eq!(classify("movq", "%rdx, 16(%rsp)").0, Class::StackStore);
    assert_eq!(classify("addq", "%rdx, (%rax)").0, Class::Store);
    assert_eq!(classify("cmpl", "$5, (%rax)").0, Class::Test);
    assert_eq!(classify("leaq", "8(%rsp), %rdi").0, Class::Alu);
    assert_eq!(classify("jne", ".LBB0_3"), (Class::Branch, Some(".LBB0_3".into())));
    assert_eq!(classify("callq", "*%rax").0, Class::IndirectCall);
    assert_eq!(demangle_legacy("__ZN15gravity_kernels14par_scan_count14par_scan_count17h0123456789abcdefE").as_deref(), Some("gravity_kernels::par_scan_count::par_scan_count"));
    assert_eq!(demangle_legacy("_ZN4core3ptr13drop_in_place17h0000000000000000E").as_deref(), Some("core::ptr::drop_in_place"));
}

#[test]
fn loops_are_backward_branches_and_hot_means_a_read_without_an_indirect_call() {
    let asm = "\t.globl _f\n_f:\n\tstp\tx29, x30, [sp, #-16]!\nLBB0_1:\n\tldr\tx8, [x0]\n\tblr\tx8\nLBB0_2:\n\tldr\tw9, [x1], #4\n\tadd\tw10, w10, w9\n\tsubs\tw11, w11, #1\n\tb.ne\tLBB0_2\n\tcbnz\tw10, LBB0_1\n\tret\n\t.cfi_endproc\n";
    let fs = parse_functions(asm);
    assert_eq!(fs.len(), 1);
    let ls = loops(&fs[0]);
    assert_eq!(ls.len(), 2);
    let inner = ls.iter().find(|l| l.label == "LBB0_2").unwrap();
    let outer = ls.iter().find(|l| l.label == "LBB0_1").unwrap();
    assert!(inner.innermost && inner.hot && !inner.window);
    assert!(!outer.innermost && !outer.hot);
    let text = cut(&fs[0], "f", "f", "test");
    assert!(text.contains("## hot LBB0_2"));
    let (findings, hot) = corpus_findings("f", &text);
    assert!(findings.is_empty(), "{findings:?}");
    assert_eq!(hot, vec![("LBB0_2".to_string(), 4)]);
    let bad = "_g:\nLBB1_1:\n\tldr\tx8, [sp, #8]\n\tbl\t__ZN1a1b17h0E\n\tsubs\tw11, w11, #1\n\tb.ne\tLBB1_1\n\tret\n\t.cfi_endproc\n";
    let g = parse_functions(bad);
    let text = cut(&g[0], "g", "g", "test");
    let (findings, _) = corpus_findings("g", &text);
    assert_eq!(findings.len(), 2, "{findings:?}");
    assert!(findings[0].what.starts_with("stack traffic") && findings[1].what.starts_with("call inside"));
    // a backward branch into a shared epilogue is not a loop; a tail call is followed to the callee
    let tail = "__ZN15gravity_kernels2cb5frame9cb_frame117h1111111111111111E:\n\tb\t__ZN15gravity_kernels2cb5frame3run17h2222222222222222E\n\t.cfi_endproc\n__ZN15gravity_kernels2cb5frame3run17h2222222222222222E:\n\tcbz\tx0, LBB2_3\nLBB2_1:\n\tldr\tw9, [x1], #4\n\tsubs\tw11, w11, #1\n\tb.ne\tLBB2_1\nLBB2_3:\n\tldp\tx29, x30, [sp, #16]\n\tret\nLBB2_4:\n\tmov\tx0, #1\n\tb\tLBB2_3\n\t.cfi_endproc\n";
    let fs = parse_functions(tail);
    assert_eq!(fs.len(), 2);
    assert!(loops(&fs[0]).is_empty());
    let r = resolve(&fs, &fs[0]);
    assert_eq!(r.demangled, "gravity_kernels::cb::frame::run");
    let ls = loops(r);
    assert_eq!(ls.iter().map(|l| l.label.as_str()).collect::<Vec<_>>(), ["LBB2_1"], "{ls:?}");
    assert!(ls[0].hot);
}

/// `GRAVITY_ASM_BLESS=<emit-asm file>` (scripts/gravity-asm.sh) regenerates the corpus first.
#[test]
fn corpus_hot_loops_have_no_calls_or_stack_traffic() {
    let dir = corpus_dir();
    if let Ok(src) = std::env::var("GRAVITY_ASM_BLESS") {
        let asm = fs::read_to_string(&src).expect("the --emit asm file");
        let meta = std::env::var("GRAVITY_ASM_META").unwrap_or_default();
        let funcs = parse_functions(&asm);
        fs::create_dir_all(&dir).unwrap();
        for (kernel, path) in KERNELS {
            let f = funcs.iter().find(|f| f.demangled == *path || f.demangled.ends_with(&format!("::{path}"))).unwrap_or_else(|| panic!("{path} not in {src}: {} functions, e.g. {:?}", funcs.len(), funcs.iter().filter(|f| f.demangled.contains("gravity_kernels")).map(|f| &f.demangled).take(5).collect::<Vec<_>>()));
            let f = resolve(&funcs, f);
            fs::write(dir.join(format!("{kernel}.s")), cut(f, kernel, path, &meta)).unwrap();
        }
    }
    let known = known_findings();
    let mut all: Vec<String> = Vec::new();
    for (kernel, _) in KERNELS {
        let path = dir.join(format!("{kernel}.s"));
        let text = fs::read_to_string(&path).unwrap_or_else(|_| panic!("{} missing: run scripts/gravity-asm.sh", path.display()));
        let (findings, hot) = corpus_findings(kernel, &text);
        println!("{kernel}: hot loops {}", hot.iter().map(|(l, n)| format!("{l} ({n} insns)")).collect::<Vec<_>>().join(", "));
        for f in findings {
            println!("  finding: {}", f.line());
            all.push(f.line());
        }
    }
    let new: Vec<&String> = all.iter().filter(|l| !known.contains(l)).collect();
    let stale: Vec<&String> = known.iter().filter(|l| !all.contains(l)).collect();
    assert!(new.is_empty() && stale.is_empty(), "hot-loop lint (docs/gravity/expr.md §2.3):\n  new: {new:?}\n  stale in known-findings.txt: {stale:?}");
}

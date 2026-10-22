// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![cfg(feature = "upstream-oracle")]

#[path = "../../test-support/oracle.rs"]
mod oracle;
use tpde_core::ir_adaptor;
#[path = "analyzer_fixture.rs"]
mod fixture;
use fixture::*;
use ir_adaptor::*;
use mcx::MemoryContext;
use std::{
    fmt::Write as _,
    fs,
    path::Path,
    process::{Command, Stdio},
};
use tpde_core::analyzer::{Analyzer, BlockIndex};

fn cfg(name: &str, edges: &[&[u32]]) -> Program {
    let mut p = Program::new(name, edges.len());
    for (b, succs) in edges.iter().enumerate() {
        p.blocks[b].succs = succs.to_vec();
    }
    let ignored = p.value(true);
    let defs: Vec<u32> = (0..edges.len()).map(|_| p.value(false)).collect();
    for b in 0..edges.len() {
        let previous = if b == 0 { 0 } else { defs[b - 1] };
        let id = p.instruction(b as u32, &[previous, 0, ignored], &[defs[b]]);
        p.insts[id as usize].fused = b % 3 == 1;
    }
    p
}
fn corpus() -> Vec<Program> {
    let mut programs = vec![
        cfg("singleton", &[&[]]),
        cfg("diamond", &[&[2, 1], &[3], &[3], &[]]),
        cfg("fanout_sort", &[&[4, 2, 1, 3], &[5], &[5], &[5], &[5], &[]]),
        cfg("duplicate_edges", &[&[2, 1, 1, 2], &[3, 3], &[3], &[]]),
        cfg("self_loop", &[&[1], &[1, 2], &[]]),
        cfg("natural_loop", &[&[1], &[2, 3], &[1], &[]]),
        cfg(
            "nested_loops",
            &[&[1], &[2, 6], &[3, 5], &[4], &[2], &[1], &[]],
        ),
        cfg("sibling_loops", &[&[1], &[2, 3], &[1], &[4, 5], &[3], &[]]),
        cfg("irreducible", &[&[1, 2], &[2, 3], &[1, 3], &[]]),
        cfg(
            "irreducible_nested",
            &[&[1, 2], &[3], &[3, 4], &[1, 2, 4], &[2, 5], &[]],
        ),
        cfg("unreachable_cycle", &[&[1], &[], &[3], &[2]]),
        cfg("unreachable_islands", &[&[4], &[2], &[], &[4], &[]]),
    ];
    let mut phi = cfg("phi_unreachable", &[&[1, 2], &[3], &[3], &[], &[3]]);
    let result = phi.value(false);
    phi.values[result as usize].incoming = vec![(1, 3), (2, 4), (4, 6)];
    phi.blocks[3].phis.push(result);
    phi.instruction(3, &[result], &[]);
    programs.push(phi);
    let mut loop_phi = cfg("phi_backedge", &[&[1], &[2, 3], &[1], &[]]);
    let result = loop_phi.value(false);
    loop_phi.values[result as usize].incoming = vec![(0, 0), (2, 4)];
    loop_phi.blocks[1].phis.push(result);
    loop_phi.instruction(2, &[result], &[]);
    programs.push(loop_phi);
    let mut zero_phi = cfg("phi_zero_slots", &[&[1], &[]]);
    let result = zero_phi.value(false);
    zero_phi.blocks[1].phis.push(result);
    zero_phi.instruction(1, &[result], &[]);
    programs.push(zero_phi);
    let mut many_results = cfg("multi_results", &[&[1], &[]]);
    let a = many_results.value(false);
    let b = many_results.value(false);
    many_results.instruction(0, &[0, 0], &[a, b]);
    many_results.instruction(1, &[a, b, a, b], &[]);
    programs.push(many_results);
    let mut sparse = cfg("sparse_indices", &[&[1], &[]]);
    sparse.values[2].local_idx = 511;
    sparse.values[3].local_idx = 2048;
    sparse.highest = 2053;
    programs.push(sparse);
    for n in [31, 32, 33, 63, 64, 65, 127, 128, 129, 257, 513] {
        let mut p = Program::linear(n);
        p.name = format!("chain_{n}");
        programs.push(p);
    }
    for n in [17, 65, 129, 257] {
        let mut p = Program::linear(n);
        p.name = format!("nested_{n}");
        for b in 1..n - 1 {
            p.blocks[b].succs.push((n - b) as u32);
        }
        programs.push(p);
    }
    let mut random = 0x4265a3d1u32;
    for sample in 0..48 {
        let n = 8 + sample % 23;
        let mut p = Program::linear(n);
        p.name = format!("generated_{sample}");
        for b in 0..n {
            random ^= random << 13;
            random ^= random >> 17;
            random ^= random << 5;
            for extra in 0..(random % 4) {
                let target = 1 + ((random.rotate_left(extra * 7) as usize) % (n - 1));
                p.blocks[b].succs.push(target as u32);
            }
            if b % 4 == 0 {
                p.instruction(b as u32, &[0, n as u32], &[]);
            }
        }
        programs.push(p);
    }
    let mut overflow = cfg("epoch_before_wrap", &[&[1], &[]]);
    overflow.epoch_override = Some(u16::MAX - 1);
    programs.push(overflow);
    programs.push(cfg("epoch_wrap", &[&[]]));
    programs.push(cfg("reuse_after_wrap", &[&[1], &[]]));
    programs
}
fn words(out: &mut String, values: &[u32]) {
    write!(out, "{}", values.len()).unwrap();
    for value in values {
        write!(out, " {value}").unwrap();
    }
    out.push('\n');
}
fn input(programs: &[Program], highest: bool, visit: bool) -> String {
    let mut out = format!(
        "{} {} {}\n",
        u32::from(highest),
        u32::from(visit),
        programs.len()
    );
    for p in programs {
        writeln!(
            out,
            "{} {} {} {} {} {} {}",
            p.name,
            p.epoch_override.map_or(65536, u32::from),
            p.entry,
            p.highest,
            p.blocks.len(),
            p.values.len(),
            p.insts.len()
        )
        .unwrap();
        words(&mut out, &p.args);
        for value in &p.values {
            write!(
                out,
                "{} {} {}",
                value.local_idx,
                u32::from(value.ignore),
                value.incoming.len()
            )
            .unwrap();
            for (block, value) in &value.incoming {
                write!(out, " {block} {value}").unwrap();
            }
            out.push('\n');
        }
        for block in &p.blocks {
            words(&mut out, &block.succs);
            words(&mut out, &block.insts);
            words(&mut out, &block.phis);
        }
        for inst in &p.insts {
            writeln!(out, "{}", u32::from(inst.fused)).unwrap();
            words(&mut out, &inst.operands);
            words(&mut out, &inst.results);
        }
    }
    out
}
fn rust_trace<H, const VISIT: bool>(programs: &[Program]) -> String
where
    for<'a> H: HighestValuePolicy<Fixture<'a, H, VISIT>>,
{
    let context = MemoryContext::new_bump("analyzer-oracle");
    let mut analyzer = Analyzer::<Fixture<'_, H, VISIT>>::new(context.mcx());
    let mut out = String::new();
    for p in programs {
        let fixture = Fixture::<H, VISIT>::new(p);
        if let Some(epoch) = p.epoch_override {
            analyzer.liveness_epoch = epoch;
        }
        analyzer.switch_func(&fixture, 0).unwrap();
        assert_eq!(
            p.successor_live.get(),
            0,
            "{} traversal iterator leak",
            p.name
        );
        writeln!(out, "case {}", p.name).unwrap();
        writeln!(
            out,
            "state {} {} {}",
            analyzer.liveness_epoch, analyzer.liveness_max_value, analyzer.num_insts
        )
        .unwrap();
        out.push_str("layout ");
        words(&mut out, &analyzer.block_layout);
        out.push_str("map ");
        words(&mut out, &analyzer.block_loop_map);
        writeln!(out, "loops {}", analyzer.loops.len()).unwrap();
        for (i, l) in analyzer.loops.iter().enumerate() {
            assert_eq!(analyzer.loop_from_idx(i as u32), l);
            writeln!(
                out,
                "loop {i} {} {} {} {} {} {} {}",
                l.level,
                l.parent,
                l.begin.0,
                l.end.0,
                l.num_blocks,
                l.definitions,
                l.definitions_in_childs
            )
            .unwrap();
        }
        writeln!(out, "values {}", analyzer.liveness_max_value + 1).unwrap();
        for i in 0..=analyzer.liveness_max_value {
            let l = &analyzer.liveness[i as usize];
            if l.epoch == analyzer.liveness_epoch {
                let checked = analyzer.liveness_info(tpde_base::ValLocalIdx::new(i));
                assert_eq!(checked, l);
                writeln!(
                    out,
                    "live {i} {} {} {} {} {} {}",
                    l.epoch,
                    l.first.0,
                    l.last.0,
                    l.ref_count,
                    l.lowest_common_loop,
                    u32::from(l.last_full)
                )
                .unwrap();
            } else {
                writeln!(out, "ignored {i} {}", l.epoch).unwrap();
            }
        }
        writeln!(
            out,
            "next {}",
            analyzer.block_ref(BlockIndex(analyzer.block_layout.len() as u32))
        )
        .unwrap();
        let before: Vec<_> = p
            .blocks
            .iter()
            .map(|b| (b.info.get(), b.info2.get()))
            .collect();
        out.push_str("rpo\n");
        analyzer.print_rpo(&fixture, &mut out).unwrap();
        assert_eq!(
            before,
            p.blocks
                .iter()
                .map(|b| (b.info.get(), b.info2.get()))
                .collect::<Vec<_>>(),
            "{} print_rpo clobbered metadata",
            p.name
        );
        assert_eq!(p.successor_live.get(), 0);
        out.push_str("layout_print\n");
        analyzer.print_block_layout(&fixture, &mut out).unwrap();
        out.push_str("loops_print\n");
        analyzer.print_loops(&fixture, &mut out).unwrap();
        out.push_str("liveness_print\n");
        analyzer.print_liveness(&fixture, &mut out).unwrap();
        writeln!(out, "metadata {}", p.blocks.len()).unwrap();
        for (i, b) in p.blocks.iter().enumerate() {
            let info = if b.info2.get() == 0 {
                u32::MAX
            } else {
                b.info.get()
            };
            if b.info2.get() != 0 {
                assert_eq!(analyzer.block_idx(&fixture, i as u32), BlockIndex(info));
                assert_eq!(analyzer.block_ref(BlockIndex(info)), i as u32);
                assert_eq!(
                    analyzer.block_has_multiple_incoming(&fixture, BlockIndex(info)),
                    analyzer.block_ref_has_multiple_incoming(&fixture, i as u32)
                );
                assert_eq!(
                    analyzer.block_has_phis(&fixture, BlockIndex(info)),
                    analyzer.block_ref_has_phis(&fixture, i as u32)
                );
                assert_eq!(
                    analyzer.block_loop_idx(BlockIndex(info)),
                    analyzer.block_loop_map[info as usize]
                );
            }
            writeln!(
                out,
                "block {i} {info} {} {} {}",
                b.info2.get(),
                u32::from(analyzer.block_ref_has_multiple_incoming(&fixture, i as u32)),
                u32::from(analyzer.block_ref_has_phis(&fixture, i as u32))
            )
            .unwrap();
        }
        out.push_str("end\n");
    }
    out
}
fn compare(executable: &Path, work: &Path, input: &str, expected: &str, label: &str) {
    let path = work.join("input.txt");
    fs::write(&path, input).unwrap();
    let output = Command::new(executable)
        .stdin(Stdio::from(fs::File::open(path).unwrap()))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{label}: C++ failed {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let actual = String::from_utf8(output.stdout).unwrap();
    let mut case = "unknown";
    let mut expected_lines = expected.lines();
    let mut actual_lines = actual.lines();
    for line in 1usize.. {
        let a = expected_lines.next();
        let b = actual_lines.next();
        if let Some(name) = a.and_then(|s| s.strip_prefix("case ")) {
            case = name;
        }
        assert_eq!(a, b, "{label}, case {case}, line {line}: field mismatch");
        if a.is_none() {
            break;
        }
    }
}
#[test]
fn pinned_analyzer_matches_complete_cfg_corpus() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR"));
    let work = std::env::temp_dir().join(format!("tpde-analyzer-oracle-{}", std::process::id()));
    fs::create_dir_all(&work).unwrap();
    let programs = corpus();
    let scripts = [
        (
            true,
            true,
            rust_trace::<ProvidesHighestValue, true>(&programs),
        ),
        (
            true,
            false,
            rust_trace::<ProvidesHighestValue, false>(&programs),
        ),
        (false, true, rust_trace::<NoHighestValue, true>(&programs)),
        (false, false, rust_trace::<NoHighestValue, false>(&programs)),
    ];
    for assertions in [true, false] {
        let exe = work.join(format!("oracle-{assertions}"));
        oracle::compile_cpp_with_flags(
            &base.join("tests/analyzer-reference/tpde/include"),
            &exe,
            &[
                base.join("tests/analyzer-reference/oracle.cpp"),
                base.join("tests/analyzer-reference/tpde/src/util/SmallVector.cpp"),
            ],
            assertions,
            &["-stdlib=libc++"],
        );
        for (highest, visit, trace) in &scripts {
            compare(
                &exe,
                &work,
                &input(&programs, *highest, *visit),
                trace,
                &format!("assertions={assertions} highest={highest} args={visit}"),
            );
        }
    }
    fs::remove_dir_all(work).unwrap();
    println!(
        "TPDE_ANALYZER_ORACLE PASS cases={} policies=4 cpp_modes=2 snapshots={} retained_epochs=3 active={}",
        programs.len(),
        programs.len() * 8,
        usize::from(tpde_util::asan::ADDRESS_SANITIZER_ACTIVE)
    );
}

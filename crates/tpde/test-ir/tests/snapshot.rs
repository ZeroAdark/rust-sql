// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use mcx::Mcx;
use std::fmt::Write;
use tpde_core::{
    ir_adaptor::{HighestValueIndexProvider, IrAdaptor, PhiRef},
    Analyzer, BlockIndex,
};
use tpde_test_ir::{IRBlockRef, IRFuncRef, IRValueRef, Op, TestIR, TestIRAdaptor};

fn name(text: &str) -> &str {
    if text.is_empty() {
        "-"
    } else {
        text
    }
}
fn words<T: Into<u32>>(out: &mut String, tag: &str, values: impl IntoIterator<Item = T>) {
    let values: Vec<u32> = values.into_iter().map(Into::into).collect();
    write!(out, "{} {}", tag, values.len()).unwrap();
    for value in values {
        write!(out, " {}", value).unwrap();
    }
    out.push('\n');
}
pub fn run(mode: &str, text: &str, mcx: Mcx<'_>) -> String {
    let mut ir = TestIR::new(mcx);
    if ir.parse_ir(text).is_err() {
        return String::from("parse 0\n");
    }
    let mut out = String::from("parse 1\n");
    match mode {
        "parse" => {}
        "print" => ir.print(&mut out).unwrap(),
        "records" => records(&ir, &mut out),
        "adaptor" => adaptor(&ir, &mut out),
        "analyze" => analyze(&ir, mcx, &mut out),
        _ => panic!("unknown TestIR protocol mode"),
    }
    out
}
fn records(ir: &TestIR<'_>, out: &mut String) {
    writeln!(
        out,
        "module {} {} {} {}",
        ir.functions.len(),
        ir.blocks.len(),
        ir.values.len(),
        ir.value_operands.len()
    )
    .unwrap();
    for (idx, f) in ir.functions.iter().enumerate() {
        writeln!(
            out,
            "function {} {} {} {} {} {} {} {} {}",
            idx,
            name(ir.name(f.name)),
            f.declaration as u8,
            f.local_only as u8,
            f.has_call as u8,
            f.arg_begin_idx,
            f.arg_end_idx,
            f.block_begin_idx,
            f.block_end_idx
        )
        .unwrap();
    }
    for (idx, b) in ir.blocks.iter().enumerate() {
        writeln!(
            out,
            "block {} {} {} {} {} {} {} {} {}",
            idx,
            name(ir.name(b.name)),
            b.succ_begin_idx,
            b.succ_end_idx,
            b.inst_begin_idx,
            b.phi_end_idx,
            b.inst_end_idx,
            b.block_info.get(),
            b.block_info2.get()
        )
        .unwrap();
    }
    for (idx, v) in ir.values.iter().enumerate() {
        writeln!(
            out,
            "value {} {} {} {} {} {} {} {} {}",
            idx,
            name(ir.name(v.name)),
            v.value_type as u8,
            v.op as u8,
            v.force_fixed_assignment as u8,
            v.call_func_idx,
            v.op_count,
            v.op_begin_idx,
            v.op_end_idx
        )
        .unwrap();
    }
    words(out, "operands", ir.value_operands.iter().copied());
}
fn adaptor(ir: &TestIR<'_>, out: &mut String) {
    let mut a = TestIRAdaptor::new(ir);
    a.start_compile();
    writeln!(
        out,
        "adaptor {} {} {} 1 1 {}",
        u32::MAX,
        u32::MAX,
        u32::MAX,
        a.func_count()
    )
    .unwrap();
    words(out, "funcs", a.funcs());
    words(out, "compile", a.funcs_to_compile());
    for f in a.funcs() {
        let function = ir.functions[f.0 as usize];
        writeln!(
            out,
            "function {} {} {} {} {}",
            f.0,
            name(std::str::from_utf8(a.func_link_name(f)).unwrap()),
            a.func_extern(f) as u8,
            a.func_only_local(f) as u8,
            a.func_has_weak_linkage(f) as u8
        )
        .unwrap();
        if function.declaration || function.block_begin_idx == function.block_end_idx {
            out.push_str("skip\n");
            continue;
        }
        let switched = a.switch_func(f);
        writeln!(
            out,
            "current {} {} {} {} {} {}",
            switched as u8,
            a.cur_highest_val_idx(),
            a.cur_needs_unwind_info() as u8,
            a.cur_is_vararg() as u8,
            a.cur_has_dynamic_alloca() as u8,
            a.cur_entry_block().0
        )
        .unwrap();
        words(out, "args", a.cur_args());
        for (index, value) in a.cur_args().enumerate() {
            let index = index as u32;
            writeln!(
                out,
                "arg {} {} {} {} {}",
                value.0,
                a.cur_arg_is_byval(index) as u8,
                a.cur_arg_byval_align(index),
                a.cur_arg_byval_size(index),
                a.cur_arg_is_sret(index) as u8
            )
            .unwrap();
        }
        words(out, "allocas", a.cur_static_allocas());
        let end = ir.blocks[function.block_end_idx as usize - 1].inst_end_idx;
        for index in function.arg_begin_idx..end {
            let value = IRValueRef(index);
            let alloca = ir.values[index as usize].op == Op::Alloca;
            writeln!(
                out,
                "value {} {} {} {} {} {} {}",
                index,
                name(a.value_fmt_ref(value)),
                a.val_local_idx(value).get(),
                a.val_is_phi(value) as u8,
                a.val_ignore_in_liveness_analysis(value) as u8,
                if alloca {
                    a.val_alloca_size(value)
                } else {
                    u32::MAX
                },
                if alloca {
                    a.val_alloca_align(value)
                } else {
                    u32::MAX
                }
            )
            .unwrap();
        }
        words(out, "blocks", a.cur_blocks());
        for b in a.cur_blocks() {
            let first = a.block_info(b);
            let second = a.block_info2(b);
            a.block_set_info(b, b.0 ^ 0xa5a5a5a5);
            a.block_set_info2(b, b.0 ^ 0x5a5a5a5a);
            writeln!(
                out,
                "block {} {} {} {} {} {}",
                b.0,
                name(a.block_fmt_ref(b)),
                first,
                second,
                a.block_info(b),
                a.block_info2(b)
            )
            .unwrap();
            a.block_set_info(b, first);
            a.block_set_info2(b, second);
            words(out, "succs", a.block_succs(b));
            words(out, "phis", a.block_phis(b));
            words(out, "insts", a.block_insts(b));
            for v in a.block_phis(b) {
                let phi = a.val_as_phi(v);
                writeln!(out, "phi {} {}", v.0, phi.incoming_count()).unwrap();
                for slot in 0..phi.incoming_count() {
                    let block = phi.incoming_block_for_slot(slot);
                    writeln!(
                        out,
                        "incoming {} {} {} {}",
                        slot,
                        phi.incoming_val_for_slot(slot).0,
                        block.0,
                        phi.incoming_val_for_block(block).0
                    )
                    .unwrap();
                }
                writeln!(
                    out,
                    "missing {}",
                    phi.incoming_val_for_block(IRBlockRef(u32::MAX)).0
                )
                .unwrap();
            }
            for inst in a.block_insts(b) {
                writeln!(
                    out,
                    "inst {} {} {}",
                    inst.0,
                    name(a.inst_fmt_ref(inst)),
                    a.inst_fused(inst) as u8
                )
                .unwrap();
                words(out, "operands", a.inst_operands(inst));
                words(out, "results", a.inst_results(inst));
            }
        }
        a.reset();
        writeln!(out, "reset {}", a.cur_highest_val_idx()).unwrap();
    }
    a.end_compile();
}
fn analyze(ir: &TestIR<'_>, mcx: Mcx<'_>, out: &mut String) {
    let mut a = TestIRAdaptor::new(ir);
    let mut analyzer = Analyzer::new(mcx);
    a.start_compile();
    for f in a.funcs() {
        let function = ir.functions[f.0 as usize];
        writeln!(
            out,
            "function {} {}",
            f.0,
            name(std::str::from_utf8(a.func_link_name(f)).unwrap())
        )
        .unwrap();
        if function.declaration || function.block_begin_idx == function.block_end_idx {
            out.push_str("skip\n");
            continue;
        }
        assert!(a.switch_func(f));
        analyzer.switch_func(&a, f).unwrap();
        writeln!(
            out,
            "state {} {} {}",
            analyzer.liveness_epoch, analyzer.liveness_max_value, analyzer.num_insts
        )
        .unwrap();
        words(out, "layout", analyzer.block_layout.iter().copied());
        words(out, "map", analyzer.block_loop_map.iter().copied());
        writeln!(out, "loops {}", analyzer.loops.len()).unwrap();
        for (index, info) in analyzer.loops.iter().enumerate() {
            writeln!(
                out,
                "loop {} {} {} {} {} {} {} {}",
                index,
                info.level,
                info.parent,
                info.begin.0,
                info.end.0,
                info.num_blocks,
                info.definitions,
                info.definitions_in_childs
            )
            .unwrap();
        }
        writeln!(out, "values {}", analyzer.liveness_max_value + 1).unwrap();
        for index in 0..=analyzer.liveness_max_value {
            let info = analyzer.liveness[index as usize];
            if info.epoch == analyzer.liveness_epoch {
                let checked = analyzer.liveness_info(tpde_base::ValLocalIdx::new(index));
                writeln!(
                    out,
                    "live {} {} {} {} {} {} {}",
                    index,
                    checked.epoch,
                    checked.first.0,
                    checked.last.0,
                    checked.ref_count,
                    checked.lowest_common_loop,
                    checked.last_full as u8
                )
                .unwrap();
            } else {
                writeln!(out, "ignored {} {}", index, info.epoch).unwrap();
            }
        }
        writeln!(
            out,
            "next {}",
            analyzer
                .block_ref(BlockIndex(analyzer.block_layout.len() as u32))
                .0
        )
        .unwrap();
        let before: Vec<_> = a
            .cur_blocks()
            .map(|block| (a.block_info(block), a.block_info2(block)))
            .collect();
        out.push_str("rpo\n");
        analyzer.print_rpo(&a, out).unwrap();
        for (index, block) in a.cur_blocks().enumerate() {
            assert_eq!(before[index], (a.block_info(block), a.block_info2(block)));
        }
        out.push_str("layout_print\n");
        analyzer.print_block_layout(&a, out).unwrap();
        out.push_str("loops_print\n");
        analyzer.print_loops(&a, out).unwrap();
        out.push_str("liveness_print\n");
        analyzer.print_liveness(&a, out).unwrap();
        writeln!(
            out,
            "metadata {}",
            function.block_end_idx - function.block_begin_idx
        )
        .unwrap();
        for block in a.cur_blocks() {
            writeln!(
                out,
                "block {} {} {} {} {}",
                block.0,
                if a.block_info2(block) == 0 {
                    u32::MAX
                } else {
                    a.block_info(block)
                },
                a.block_info2(block),
                analyzer.block_ref_has_multiple_incoming(&a, block) as u8,
                analyzer.block_ref_has_phis(&a, block) as u8
            )
            .unwrap();
        }
        out.push_str("end\n");
    }
    a.end_compile();
}

// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![cfg(feature = "upstream-oracle")]

#[path = "../../test-support/oracle.rs"]
mod oracle;
#[allow(dead_code)]
#[path = "../../test-support/dependencies.rs"]
mod dependencies;
use tpde_core::ir_adaptor;
#[path = "ir_fixture.rs"]
mod fixture;
use fixture::*;
use ir_adaptor::*;
use std::{
    fmt::Write as _,
    fs,
    mem::size_of,
    path::Path,
    process::{Command, Output},
};

const MISSING: &[&str] = &[
    "func_count",
    "funcs",
    "funcs_to_compile",
    "func_link_name",
    "func_extern",
    "func_only_local",
    "func_has_weak_linkage",
    "cur_needs_unwind_info",
    "cur_is_vararg",
    "cur_highest_val_idx",
    "cur_args",
    "cur_arg_is_byval",
    "cur_arg_byval_size",
    "cur_arg_byval_align",
    "cur_arg_is_sret",
    "cur_static_allocas",
    "cur_has_dynamic_alloca",
    "cur_entry_block",
    "cur_blocks",
    "block_succs",
    "block_insts",
    "block_phis",
    "block_info",
    "block_set_info",
    "block_info2",
    "block_set_info2",
    "block_fmt_ref",
    "val_local_idx",
    "val_ignore_in_liveness_analysis",
    "val_is_phi",
    "val_as_phi",
    "val_alloca_size",
    "val_alloca_align",
    "value_fmt_ref",
    "inst_operands",
    "inst_results",
    "inst_fused",
    "inst_fmt_ref",
    "start_compile",
    "end_compile",
    "switch_func",
    "reset",
];

fn concept_trace(out: &mut String) -> usize {
    let mut count = 0;
    for (name, admitted) in [
        ("complete", true),
        ("no_highest", true),
        ("eight_byte_refs", true),
        ("large_inst_ref", true),
        ("no_visit_args", true),
        ("convertible_results", true),
        ("format_integer", true),
        ("format_string", true),
        ("is_false_false", true),
        ("is_false_true", false),
        ("iter_pointer", true),
        ("iter_value_increment", true),
        ("iter_no_deref", false),
        ("iter_no_increment", false),
        ("iter_no_compare", false),
        ("end_sentinel", true),
        ("end_wrong", false),
        ("range_span", true),
        ("range_sentinel", true),
        ("range_bad", false),
        ("phi", true),
        ("phi_missing_lookup", false),
        ("phi_wrong_count", false),
        ("phi_wrong_value", false),
        ("phi_wrong_block", false),
        ("value_no_equality", false),
        ("block_no_equality", false),
        ("func_no_equality", false),
        ("value_too_large", false),
        ("block_too_large", false),
        ("func_too_large", false),
        ("invalid_inst_type", false),
        ("bad_range_adapter", false),
        ("bad_phi_adapter", false),
    ] {
        writeln!(out, "concept {name} {}", u32::from(admitted)).unwrap();
        count += 1;
    }
    for name in MISSING {
        writeln!(out, "concept missing_{name} 0").unwrap();
        count += 1;
    }
    for name in [
        "cur_arg_byval_size",
        "cur_arg_byval_align",
        "block_info",
        "block_info2",
        "INVALID_VALUE_REF",
        "INVALID_BLOCK_REF",
        "INVALID_FUNC_REF",
        "TPDE_PROVIDES_HIGHEST_VAL_IDX",
        "TPDE_LIVENESS_VISIT_ARGS",
    ] {
        writeln!(out, "concept wrong_{name} 0").unwrap();
        count += 1;
    }
    count
}

fn api_trace(out: &mut String) {
    let mut a = Adaptor::new(<Fixture<'_>>::new());
    writeln!(
        out,
        "refs {} {} {} {}",
        size_of::<<Fixture<'_> as IrAdaptor>::ValueRef>(),
        size_of::<<Fixture<'_> as IrAdaptor>::InstRef>(),
        size_of::<<Fixture<'_> as IrAdaptor>::BlockRef>(),
        size_of::<<Fixture<'_> as IrAdaptor>::FuncRef>()
    )
    .unwrap();
    writeln!(
        out,
        "sentinels {} {} {}",
        <Fixture<'_>>::INVALID_VALUE_REF,
        <Fixture<'_>>::INVALID_BLOCK_REF,
        <Fixture<'_>>::INVALID_FUNC_REF
    )
    .unwrap();
    writeln!(
        out,
        "flags {} {}",
        u32::from(Adaptor::<Fixture<'_>>::TPDE_PROVIDES_HIGHEST_VAL_IDX),
        u32::from(<Fixture<'_>>::TPDE_LIVENESS_VISIT_ARGS)
    )
    .unwrap();
    writeln!(out, "module {}", a.func_count()).unwrap();
    for f in a.funcs() {
        writeln!(
            out,
            "func {f} {} {} {} {}",
            std::str::from_utf8(a.func_link_name(f)).unwrap(),
            u32::from(a.func_extern(f)),
            u32::from(a.func_only_local(f)),
            u32::from(a.func_has_weak_linkage(f))
        )
        .unwrap();
    }
    a.start_compile();
    for f in a.funcs_to_compile() {
        writeln!(out, "compile {f}").unwrap();
        writeln!(out, "switch {f} {}", u32::from(a.switch_func(f))).unwrap();
    }
    writeln!(
        out,
        "current {} {} {} {} {}",
        u32::from(a.cur_needs_unwind_info()),
        u32::from(a.cur_is_vararg()),
        a.highest_value_index().unwrap(),
        u32::from(a.cur_has_dynamic_alloca()),
        a.cur_entry_block()
    )
    .unwrap();
    for (index, value) in a.cur_args().enumerate() {
        let index = index as u32;
        writeln!(
            out,
            "arg {index} {value} {} {} {} {}",
            u32::from(a.cur_arg_is_byval(index)),
            a.cur_arg_byval_size(index),
            a.cur_arg_byval_align(index),
            u32::from(a.cur_arg_is_sret(index))
        )
        .unwrap();
    }
    for value in a.cur_static_allocas() {
        writeln!(
            out,
            "alloca {value} {} {}",
            a.val_alloca_size(value),
            a.val_alloca_align(value)
        )
        .unwrap();
    }
    for block in a.cur_blocks() {
        a.block_set_info(block, 100 + block);
        a.block_set_info2(block, 200 + block);
        writeln!(
            out,
            "block {block} {} {} {}",
            a.block_info(block),
            a.block_info2(block),
            a.block_fmt_ref(block)
        )
        .unwrap();
        for successor in a.block_succs(block) {
            writeln!(out, "succ {block} {successor}").unwrap();
        }
        for value in a.block_phis(block) {
            let phi = a.val_as_phi(value);
            writeln!(out, "phi {value} {}", phi.incoming_count()).unwrap();
            for slot in 0..phi.incoming_count() {
                let predecessor = phi.incoming_block_for_slot(slot);
                writeln!(
                    out,
                    "incoming {slot} {} {predecessor} {}",
                    phi.incoming_val_for_slot(slot),
                    phi.incoming_val_for_block(predecessor)
                )
                .unwrap();
            }
        }
        for inst in a.block_insts(block) {
            writeln!(
                out,
                "inst {inst} {} {}",
                u32::from(a.inst_fused(inst)),
                a.inst_fmt_ref(inst)
            )
            .unwrap();
            for operand in a.inst_operands(inst) {
                writeln!(out, "operand {inst} {operand}").unwrap();
            }
            for result in a.inst_results(inst) {
                writeln!(out, "result {inst} {result}").unwrap();
            }
        }
    }
    for value in 10..=15 {
        writeln!(
            out,
            "value {value} {} {} {} {}",
            a.val_local_idx(value).get(),
            u32::from(a.val_ignore_in_liveness_analysis(value)),
            u32::from(a.val_is_phi(value)),
            a.value_fmt_ref(value)
        )
        .unwrap();
    }
    writeln!(out, "switch 1 {}", u32::from(a.switch_func(1))).unwrap();
    a.end_compile();
    a.reset();
    writeln!(
        out,
        "reset {} {} {} {} {}",
        a.block_info(1),
        a.block_info2(1),
        a.block_info(2),
        a.block_info2(2),
        a.current
    )
    .unwrap();
    a.start_compile();
    writeln!(out, "recompile {}", u32::from(a.switch_func(0))).unwrap();
    a.end_compile();
    writeln!(
        out,
        "lifecycle {} {} {} {}",
        a.started, a.ended, a.switches, a.resets
    )
    .unwrap();
}

#[test]
fn pinned_concepts_and_complete_callback_trace() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR"));
    let work = std::env::temp_dir().join(format!("tpde-ir-oracle-{}", std::process::id()));
    fs::create_dir_all(&work).unwrap();
    let mut expected = String::new();
    let concepts = concept_trace(&mut expected);
    api_trace(&mut expected);
    for assertions in [true, false] {
        let executable = work.join(format!("oracle-{assertions}"));
        oracle::compile_cpp_with_flags(
            &base.join("tests/ir-reference/tpde/include"),
            &executable,
            &[base.join("tests/ir_adaptor_oracle.cpp")],
            assertions,
            &["-stdlib=libc++"],
        );
        oracle::compare_bytes(
            &executable,
            &work,
            &[],
            expected.as_bytes(),
            "IRAdaptor concepts and callbacks",
        );
    }
    fs::remove_dir_all(&work).unwrap();
    println!(
        "TPDE_IR_ADAPTOR_ORACLE PASS concepts={concepts} cpp_modes=2 trace_lines={}",
        expected.lines().count() - concepts
    );
}


fn compile_fixture(work: &Path, deps: &Path, name: &str, fixture: &str, main: &str) -> Output {
    let path = work.join(format!("{name}.rs"));
    fs::write(&path, format!("#![allow(dead_code,unused_imports)]\nuse tpde_core::ir_adaptor;\n{fixture}\nfn main() {{ {main} }}\n")).unwrap();
    let mut command = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()));
    command
        .args(["--edition=2021", "--emit=obj", "--crate-name", name])
        .arg("-L")
        .arg(format!("dependency={}", deps.display()));
    for name in ["tpde_core", "tpde_base"] {
        for argument in dependencies::extern_arguments(deps, name) {
            command.arg("--extern").arg(argument);
        }
    }
    command
        .arg(&path)
        .arg("-o")
        .arg(work.join(format!("{name}.o")))
        .output()
        .unwrap()
}

fn without_method(source: &str, name: &str) -> String {
    let start = source
        .find(&format!("    fn {name}("))
        .expect("fixture method");
    let brace = start + source[start..].find('{').unwrap();
    let mut depth = 0;
    let end = source[brace..]
        .char_indices()
        .find_map(|(offset, c)| {
            if c == '{' {
                depth += 1;
            }
            if c == '}' {
                depth -= 1;
            }
            (depth == 0).then_some(brace + offset + 1)
        })
        .unwrap();
    format!("{}{}", &source[..start], &source[end..])
}

#[test]
fn rust_contract_admission_and_rejection() {
    let work = std::env::temp_dir().join(format!("tpde-ir-contract-{}", std::process::id()));
    fs::create_dir_all(&work).unwrap();
    let deps = dependencies::current_dependency_directory(&work, &["tpde_core", "tpde_base"]);
    let source = include_str!("ir_fixture.rs");
    let valid = "let _ = Adaptor::new(<Fixture<'_>>::new());";
    let mut cases: Vec<(String, String, String, bool, String)> = vec![
        (
            "complete".into(),
            source.into(),
            valid.into(),
            true,
            String::new(),
        ),
        (
            "no_highest".into(),
            source.into(),
            "let _ = Adaptor::new(<Fixture<'_,u32,u32,u32,NoHighestValue>>::new());".into(),
            true,
            String::new(),
        ),
        (
            "eight_byte_refs".into(),
            source.into(),
            "let _ = Adaptor::new(<Fixture<'_,u64,u64,u64>>::new());".into(),
            true,
            String::new(),
        ),
        (
            "large_inst_ref".into(),
            source.into(),
            "let _ = Adaptor::new(<Fixture<'_,u32,u32,u32,ProvidesHighestValue,LargeRef>>::new());"
                .into(),
            true,
            String::new(),
        ),
        (
            "no_visit_args".into(),
            source.replace(
                "TPDE_LIVENESS_VISIT_ARGS: bool = true",
                "TPDE_LIVENESS_VISIT_ARGS: bool = false",
            ),
            valid.into(),
            true,
            String::new(),
        ),
    ];
    for name in MISSING {
        cases.push((
            format!("missing_{name}"),
            without_method(source, name),
            valid.into(),
            false,
            format!("`{name}`"),
        ));
    }
    for (name, parameters) in [
        ("value", "LargeRef,u32,u32"),
        ("block", "u32,LargeRef,u32"),
        ("function", "u32,u32,LargeRef"),
    ] {
        cases.push((
            format!("large_{name}"),
            source.into(),
            format!("let _ = Adaptor::new(<Fixture<'_,{parameters}>>::new());"),
            false,
            format!("IR {name} reference exceeds 8 bytes"),
        ));
    }
    for name in ["INVALID_VALUE_REF", "INVALID_BLOCK_REF", "INVALID_FUNC_REF"] {
        let original = format!("const {name}: ");
        let start = source.find(&original).unwrap();
        let end = start + source[start..].find(';').unwrap() + 1;
        let mutated = format!(
            "{}const {name}: u8 = 0;{}",
            &source[..start],
            &source[end..]
        );
        cases.push((
            format!("wrong_{}", name.to_lowercase()),
            mutated,
            valid.into(),
            false,
            name.into(),
        ));
    }
    cases.push((
        "wrong_flag".into(),
        source.replace(
            "TPDE_LIVENESS_VISIT_ARGS: bool = true",
            "TPDE_LIVENESS_VISIT_ARGS: u32 = 1",
        ),
        valid.into(),
        false,
        "TPDE_LIVENESS_VISIT_ARGS".into(),
    ));
    cases.push((
        "missing_capability".into(),
        source.replace("type HighestValue = H;", "type HighestValue = u32;"),
        valid.into(),
        false,
        "HighestValuePolicy".into(),
    ));
    for name in [
        "cur_arg_byval_size",
        "cur_arg_byval_align",
        "block_info",
        "block_info2",
    ] {
        let begin = source.find(&format!("    fn {name}(")).unwrap();
        let offset = begin + source[begin..].find("-> u32").unwrap();
        let mutated = format!(
            "{}-> u64{}",
            &source[..offset],
            &source[offset + "-> u32".len()..]
        );
        cases.push((
            format!("wrong_{name}"),
            mutated,
            valid.into(),
            false,
            name.into(),
        ));
    }
    for name in [
        "incoming_count",
        "incoming_val_for_slot",
        "incoming_block_for_slot",
        "incoming_val_for_block",
    ] {
        cases.push((
            format!("missing_phi_{name}"),
            without_method(source, name),
            valid.into(),
            false,
            name.into(),
        ));
    }
    cases.push((
        "range_wrong_item".into(),
        source.into(),
        "fn range<R: IrRange<u32>>(_: R) {} range([0u64]);".into(),
        false,
        "u32".into(),
    ));
    cases.push((
        "iterator_missing_next".into(),
        source.into(),
        "struct Iter; impl Iterator for Iter { type Item=u32; }".into(),
        false,
        "next".into(),
    ));
    cases.push((
        "non_formatter".into(),
        source.into(),
        "struct Nope; fn format<T: CanBeFormatted>(_:T) {} format(Nope);".into(),
        false,
        "Display".into(),
    ));
    cases.push(("borrowed_range_reset".into(), source.into(), "let mut a=Adaptor::new(<Fixture<'_>>::new()); let r=a.cur_blocks(); a.reset(); let _=r.count();".into(), false, "E0502".into()));
    cases.push(("borrowed_phi_reset".into(), source.into(), "let mut a=Adaptor::new(<Fixture<'_>>::new()); let p=a.val_as_phi(13); a.reset(); let _=p.incoming_count();".into(), false, "E0502".into()));
    cases.push(("borrowed_link_name_reset".into(), source.into(), "let mut a=Adaptor::new(<Fixture<'_>>::new()); let n=a.func_link_name(0); a.reset(); let _=n.len();".into(), false, "E0502".into()));
    cases.push(("lifetime_escape".into(), source.into(), "let a; { let name=vec![0u8]; let mut f=<Fixture<'_>>::new(); f.names=[&name,&name]; a=Adaptor::new(f); } let _=a.func_link_name(0);".into(), false, "E0597".into()));
    for batch in cases.chunks(4) {
        std::thread::scope(|scope| {
            for (name, source, main, admitted, diagnostic) in batch {
                let work = &work;
                let deps = &deps;
                scope.spawn(move || {
                    let output = compile_fixture(work, deps, name, source, main);
                    let errors = String::from_utf8_lossy(&output.stderr);
                    assert_eq!(output.status.success(), *admitted, "{name}: {errors}");
                    if !admitted {
                        assert!(
                            errors.contains(diagnostic),
                            "{name}: expected {diagnostic}: {errors}"
                        );
                    }
                });
            }
        });
    }
    fs::remove_dir_all(&work).unwrap();
    println!("TPDE_IR_ADAPTOR_RUST_CONTRACT PASS cases={}", cases.len());
}

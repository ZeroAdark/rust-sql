// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![cfg(feature = "upstream-oracle")]

#[allow(dead_code)]
#[path = "../../test-support/dependencies.rs"]
mod dependencies;

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const CONFIG: &str = "impl CompilerConfig for TestConfig";
const POLICY: &str = "impl CCAssignmentPolicy for TestPolicy";
const PARTS: &str = "impl ValueParts for Parts";
const COMPILER: &str = "impl<'cc> Compiler<TestConfig> for Fixture<'cc>";
const CUSTOM: &str = "impl CustomVariableReferenceCompiler<TestConfig> for Fixture<'_>";
const ADMIT: &str = "let info = CCInfo::new(15, 3, 12); let mut compiler = Fixture::new(&info); tpde_core::compiler::check_compiler::<TestConfig, Fixture<'_>>(); std::hint::black_box(&mut compiler);";

struct Case {
    name: String,
    source: String,
    main: String,
    diagnostic: Vec<String>,
}

impl Case {
    fn positive(name: &str, source: String, main: &str) -> Self {
        Self {
            name: name.into(),
            source,
            main: main.into(),
            diagnostic: Vec::new(),
        }
    }

    fn negative(name: &str, source: String, main: &str, diagnostic: &[&str]) -> Self {
        Self {
            name: name.into(),
            source,
            main: main.into(),
            diagnostic: diagnostic.iter().map(|value| (*value).into()).collect(),
        }
    }
}

fn block_end(source: &str, start: usize) -> usize {
    let brace = start + source[start..].find('{').expect("fixture opening brace");
    let mut depth = 0;
    for (offset, ch) in source[brace..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return brace + offset + 1;
                }
            }
            _ => {}
        }
    }
    panic!("fixture closing brace");
}

fn replace_item(source: &str, scope: &str, kind: &str, name: &str, replacement: &str) -> String {
    let scope_start = source.find(scope).expect("fixture implementation");
    let scope_end = block_end(source, scope_start);
    let needle = format!("{kind} {name}");
    let offset = source[scope_start..scope_end]
        .match_indices(&needle)
        .find(|(offset, _)| {
            let next = source.as_bytes()[scope_start + offset + needle.len()];
            matches!(next, b'(' | b'<' | b':' | b' ' | b'=')
        })
        .map(|(offset, _)| offset)
        .expect("fixture item");
    let start = scope_start + offset;
    let end = if kind == "fn" {
        block_end(source, start)
    } else {
        let mut brackets = 0;
        start
            + source[start..scope_end]
                .char_indices()
                .find_map(|(offset, ch)| {
                    match ch {
                        '[' | '(' | '{' => brackets += 1,
                        ']' | ')' | '}' => brackets -= 1,
                        ';' if brackets == 0 => return Some(offset),
                        _ => {}
                    }
                    None
                })
                .expect("fixture item end")
            + 1
    };
    assert!(end <= scope_end);
    format!("{}{replacement}{}", &source[..start], &source[end..])
}

fn without_impl(source: &str, scope: &str) -> String {
    let start = source.find(scope).expect("fixture implementation");
    let end = block_end(source, start);
    format!("{}{}", &source[..start], &source[end..])
}

fn custom_source(source: &str) -> String {
    let source = replace_item(
        source,
        CONFIG,
        "const",
        "DEFAULT_VAR_REF_HANDLING",
        "const DEFAULT_VAR_REF_HANDLING: bool = false;",
    );
    replace_item(
        &source,
        CONFIG,
        "type",
        "VariableReferences",
        "type VariableReferences = tpde_core::compiler_config::CustomVariableReferences;",
    )
}

fn compile(work: &Path, deps: &Path, libraries: &[(String, PathBuf)], case: &Case) -> Output {
    let path = work.join(format!("{}.rs", case.name));
    fs::write(
        &path,
        format!(
            "#![allow(dead_code, unused_imports, unused_variables, unused_mut, unused_assignments)]\n{}\nfn main() {{ {} }}\n",
            case.source, case.main
        ),
    )
    .expect("write contract fixture");
    let mut command = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()));
    command
        .args([
            "--edition=2021",
            "--emit=obj",
            "--color=never",
            "--crate-name",
        ])
        .arg(&case.name)
        .arg("-L")
        .arg(format!("dependency={}", deps.display()));
    for (name, path) in libraries {
        command
            .arg("--extern")
            .arg(format!("{name}={}", path.display()));
    }
    command
        .arg(path)
        .arg("-o")
        .arg(work.join(format!("{}.o", case.name)))
        .output()
        .expect("start Rust compiler for contract fixture")
}

fn verify(work: &Path, deps: &Path, libraries: &[(String, PathBuf)], cases: &[Case]) {
    for batch in cases.chunks(4) {
        std::thread::scope(|scope| {
            for case in batch {
                scope.spawn(move || {
                    let output = compile(work, deps, libraries, case);
                    let errors = String::from_utf8_lossy(&output.stderr);
                    fs::write(
                        work.join(format!("{}.stderr", case.name)),
                        errors.as_bytes(),
                    )
                    .expect("write contract diagnostics");
                    let admitted = case.diagnostic.is_empty();
                    assert_eq!(output.status.success(), admitted, "{}: {errors}", case.name);
                    if !admitted {
                        let matched = errors.split("\nerror").any(|diagnostic| {
                            case.diagnostic
                                .iter()
                                .all(|needle| diagnostic.contains(needle))
                        });
                        assert!(
                            matched,
                            "{}: expected one diagnostic containing {:?}: {errors}",
                            case.name, case.diagnostic
                        );
                        for infrastructure in
                            ["E0460", "E0463", "E0519", "incompatible version of rustc"]
                        {
                            assert!(
                                !errors.contains(infrastructure),
                                "{}: dependency failure: {errors}",
                                case.name
                            );
                        }
                    }
                    println!(
                        "TPDE_COMPILER_CONTRACT_CASE PASS kind={} name={}",
                        if admitted { "positive" } else { "negative" },
                        case.name
                    );
                });
            }
        });
    }
}

#[test]
fn production_compiler_contract_admission_and_refusals() {
    let source = include_str!("compiler_contracts_fixture.rs");
    let custom = custom_source(source);
    let no_custom = without_impl(source, CUSTOM);
    let zero_banks = replace_item(
        source,
        CONFIG,
        "const",
        "NUM_BANKS",
        "const NUM_BANKS: u32 = 0;",
    );
    let zero_banks = replace_item(
        &zero_banks,
        CONFIG,
        "type",
        "Banks",
        "type Banks = Banks<0>;",
    );
    let zero_banks = replace_item(
        &zero_banks,
        COMPILER,
        "const",
        "NUM_FIXED_ASSIGNMENTS",
        "const NUM_FIXED_ASSIGNMENTS: [u32; 0] = [];",
    );
    let edge_config = replace_item(
        source,
        CONFIG,
        "const",
        "PLATFORM_POINTER_SIZE",
        "const PLATFORM_POINTER_SIZE: u32 = 0;",
    );
    let edge_config = replace_item(
        &edge_config,
        CONFIG,
        "const",
        "FRAME_INDEXING_NEGATIVE",
        "const FRAME_INDEXING_NEGATIVE: bool = false;",
    );
    let edge_config = replace_item(
        &edge_config,
        CONFIG,
        "type",
        "Assembler",
        "type Assembler = ();",
    );
    let edge_config = replace_item(
        &edge_config,
        CONFIG,
        "type",
        "FunctionWriter",
        "type FunctionWriter = ();",
    );
    let positives = vec![
        Case::positive("complete", source.into(), ADMIT),
        Case::positive("default_without_custom_helpers", no_custom.clone(), ADMIT),
        Case::positive("custom_with_both_helpers", custom.clone(), ADMIT),
        Case::positive("config_zero_banks", zero_banks.replace("tpde_core::compiler::check_compiler::<TestConfig, Self>();", ""), "tpde_core::compiler_config::check_config::<TestConfig>();"),
        Case::positive("unconstrained_configuration_values", edge_config, ADMIT),
        Case::positive("maximum_pointer_size", replace_item(source, CONFIG, "const", "PLATFORM_POINTER_SIZE", "const PLATFORM_POINTER_SIZE: u32 = u32::MAX;"), ADMIT),
        Case::positive("non_copy_no_drop_policy", source.into(), "let info = CCInfo::new(0, 0, 0); let mut current = CCAssigner::new(&info, TestPolicy::default()); current.reset(); let mut assignment = CCAssignment::new(); assignment.bank = RegBank::new(0); current.assign_arg(&mut assignment); current.assign_ret(&mut assignment); let _: u32 = current.get_stack_size(); let _: bool = current.is_vararg();"),
        Case::positive("same_owner_generic_prologue", source.into(), "fn selected<T: Compiler<TestConfig>>(compiler: &mut T, value: T::IRValueRef) { tpde_core::compiler::check_compiler::<TestConfig, T>(); compiler.with_cur_cc_assigner(|compiler, current| { current.reset(); compiler.prologue_assign_arg(current, 2, value); }); } let info = CCInfo::new(15, 3, 12); let mut compiler = Fixture::new(&info); selected(&mut compiler, 19);"),
        Case::positive("live_table_retarget", source.into(), "let first = CCInfo::new(1, 2, 3); let second = CCInfo::with_red_zone(4, 5, 6, u8::MAX); let mut current = CCAssigner::new(&first, TestPolicy::default()); current.ccinfo = &second; let _: u64 = current.get_ccinfo().allocatable_regs(); let _: u64 = current.get_ccinfo().callee_saved_regs(); let _: u64 = current.get_ccinfo().arg_regs(); let _: u8 = current.get_ccinfo().red_zone_size();"),
        Case::positive("table_outlives_assigner", source.into(), "let info = CCInfo::new(1, 2, 3); let table; { let current = CCAssigner::new(&info, TestPolicy::default()); table = current.get_ccinfo(); } std::hint::black_box(table.allocatable_regs());"),
        Case::positive("replace_table_after_last_use", source.into(), "let mut info = CCInfo::new(1, 2, 3); let current = CCAssigner::new(&info, TestPolicy::default()); std::hint::black_box(current.get_ccinfo()); info = CCInfo::new(4, 5, 6); std::hint::black_box(info);"),
        Case::positive("special_mode_and_payload_boundaries", source.into(), "use tpde_core::compiler::ValRefSpecialStruct; #[repr(align(64))] #[derive(Clone, Copy)] struct Aligned(u8); let mut special = ValRefSpecial::new(0, ()); *special.mode_mut() = 3; let _: &mut u8 = special.mode_mut(); let _: u8 = special.mode(); let _: ValRefSpecial<Aligned> = ValRefSpecial::new(u8::MAX, Aligned(0)); const { assert!(core::mem::offset_of!(ValRefSpecial<Aligned>, mode) == 0); };"),
        Case::positive("table_reference_escapes_callback", source.into(), "let info = CCInfo::new(1, 2, 3); let mut compiler = Fixture::new(&info); let table = compiler.with_cur_cc_assigner(|_, current| current.get_ccinfo()); compiler.start_func(0); std::hint::black_box(table.allocatable_regs());"),
    ];

    let mut negatives = Vec::new();
    negatives.push(Case::negative(
        "compiler_zero_banks",
        zero_banks,
        ADMIT,
        &["E0080", "compiler requires at least one register bank"],
    ));
    for name in [
        "FRAME_INDEXING_NEGATIVE",
        "PLATFORM_POINTER_SIZE",
        "NUM_BANKS",
        "DEFAULT_VAR_REF_HANDLING",
    ] {
        negatives.push(Case::negative(
            &format!("missing_config_{}", name.to_lowercase()),
            replace_item(source, CONFIG, "const", name, ""),
            ADMIT,
            &["E0046", "not all trait items implemented", name],
        ));
    }
    for name in [
        "DefaultCCAssigner",
        "Assembler",
        "AsmReg",
        "FunctionWriter",
        "Banks",
        "VariableReferences",
    ] {
        negatives.push(Case::negative(
            &format!("missing_config_{name}"),
            replace_item(source, CONFIG, "type", name, ""),
            ADMIT,
            &["E0046", "not all trait items implemented", name],
        ));
    }
    for (name, declaration) in [
        (
            "FRAME_INDEXING_NEGATIVE",
            "const FRAME_INDEXING_NEGATIVE: u8 = 1;",
        ),
        (
            "PLATFORM_POINTER_SIZE",
            "const PLATFORM_POINTER_SIZE: u64 = 8;",
        ),
        ("NUM_BANKS", "const NUM_BANKS: usize = 2;"),
        (
            "DEFAULT_VAR_REF_HANDLING",
            "const DEFAULT_VAR_REF_HANDLING: u32 = 1;",
        ),
    ] {
        negatives.push(Case::negative(
            &format!("wrong_config_{}", name.to_lowercase()),
            replace_item(source, CONFIG, "const", name, declaration),
            ADMIT,
            &["E0326", "incompatible type for trait", name],
        ));
    }
    negatives.push(Case::negative(
        "bank_count_disagreement",
        replace_item(
            source,
            CONFIG,
            "const",
            "NUM_BANKS",
            "const NUM_BANKS: u32 = 3;",
        ),
        ADMIT,
        &["E0080", "compiler register bank count mismatch"],
    ));
    negatives.push(Case::negative(
        "default_policy_flag_disagreement",
        replace_item(
            source,
            CONFIG,
            "const",
            "DEFAULT_VAR_REF_HANDLING",
            "const DEFAULT_VAR_REF_HANDLING: bool = false;",
        ),
        ADMIT,
        &["E0080", "compiler variable reference policy mismatch"],
    ));
    negatives.push(Case::negative(
        "custom_policy_flag_disagreement",
        replace_item(
            &custom,
            CONFIG,
            "const",
            "DEFAULT_VAR_REF_HANDLING",
            "const DEFAULT_VAR_REF_HANDLING: bool = true;",
        ),
        ADMIT,
        &["E0080", "compiler variable reference policy mismatch"],
    ));
    negatives.push(Case::negative(
        "incomplete_default_configuration",
        source.into(),
        "tpde_core::compiler_config::check_config::<CompilerConfigDefault>();",
        &["E0277", "CompilerConfig", "CompilerConfigDefault"],
    ));
    negatives.push(Case::negative(
        "fake_default_assigner",
        replace_item(
            source,
            CONFIG,
            "type",
            "DefaultCCAssigner",
            "type DefaultCCAssigner<'cc> = ();",
        ),
        ADMIT,
        &["E0277", "CallingConventionAssigner"],
    ));
    negatives.push(Case::negative("unsealed_register_banks", source.into(), "struct Fake; impl tpde_core::compiler_config::RegisterBanks for Fake { type FixedAssignments = [u32; 2]; const COUNT: u32 = 2; }", &["E0277", "sealed::Banks", "Fake"]));
    negatives.push(Case::negative("unsealed_variable_policy", source.into(), "struct Fake; impl tpde_core::compiler_config::VariableReferencePolicy for Fake { const DEFAULT: bool = true; }", &["E0277", "VariableRefs", "Fake"]));

    for name in ["reset", "assign_arg", "get_stack_size", "assign_ret"] {
        negatives.push(Case::negative(
            &format!("missing_policy_{name}"),
            replace_item(source, POLICY, "fn", name, ""),
            ADMIT,
            &["E0046", "not all trait items implemented", name],
        ));
    }
    for (name, declaration) in [
        (
            "get_stack_size",
            "fn get_stack_size(&mut self) -> u64 { 0 }",
        ),
        (
            "assign_arg",
            "fn assign_arg(&mut self, _: &CCAssignment) {}",
        ),
        (
            "assign_ret",
            "fn assign_ret(&mut self, _: &CCAssignment) {}",
        ),
    ] {
        negatives.push(Case::negative(
            &format!("wrong_policy_{name}"),
            replace_item(source, POLICY, "fn", name, declaration),
            ADMIT,
            &["E0053", "incompatible type for trait", name],
        ));
    }
    negatives.push(Case::negative(
        "readonly_policy_stack_query",
        replace_item(
            source,
            POLICY,
            "fn",
            "get_stack_size",
            "fn get_stack_size(&self) -> u32 { 0 }",
        ),
        ADMIT,
        &["E0053", "incompatible type for trait", "get_stack_size"],
    ));
    negatives.push(Case::negative("wrong_policy_vararg_result", format!("{source}\nstruct WrongVararg; impl CCAssignmentPolicy for WrongVararg {{ fn reset(&mut self) {{}} fn assign_arg(&mut self, _: &mut CCAssignment) {{}} fn assign_ret(&mut self, _: &mut CCAssignment) {{}} fn get_stack_size(&mut self) -> u32 {{ 0 }} fn is_vararg(&self) -> u8 {{ 0 }} }}"), ADMIT, &["E0053", "incompatible type for trait", "is_vararg"]));
    negatives.push(Case::negative("unsealed_assigner", source.into(), "struct Fake; impl CallingConventionAssigner for Fake { fn get_ccinfo(&self) -> &CCInfo { panic!() } fn reset(&mut self) {} fn assign_arg(&mut self, _: &mut CCAssignment) {} fn get_stack_size(&mut self) -> u32 { 0 } fn is_vararg(&self) -> bool { false } fn assign_ret(&mut self, _: &mut CCAssignment) {} }", &["E0277", "sealed::Assigner", "Fake"]));
    negatives.push(Case::negative(
        "explicit_drop_policy",
        format!("{source}\nimpl Drop for TestPolicy {{ fn drop(&mut self) {{}} }}"),
        ADMIT,
        &["E0080", "CC assigner policy must not need drop"],
    ));
    let owned_policy = format!("{source}\nstruct OwnedPolicy(String); impl CCAssignmentPolicy for OwnedPolicy {{ fn reset(&mut self) {{}} fn assign_arg(&mut self, _: &mut CCAssignment) {{}} fn assign_ret(&mut self, _: &mut CCAssignment) {{}} fn get_stack_size(&mut self) -> u32 {{ 0 }} }}");
    negatives.push(Case::negative("owned_drop_policy", owned_policy.clone(), "let info = CCInfo::new(0, 0, 0); let current = CCAssigner::new(&info, OwnedPolicy(String::new())); std::hint::black_box(current);", &["E0080", "CC assigner policy must not need drop"]));
    negatives.push(Case::negative("private_policy_constructor_bypass", owned_policy, "let info = CCInfo::new(0, 0, 0); let current = CCAssigner { ccinfo: &info, policy: OwnedPolicy(String::new()) }; std::hint::black_box(current);", &["E0451", "policy", "private"]));
    for field in [
        "allocatable_regs",
        "callee_saved_regs",
        "arg_regs",
        "red_zone_size",
    ] {
        negatives.push(Case::negative(&format!("immutable_ccinfo_{field}"), source.into(), &format!("let mut info = CCInfo::new(1, 2, 3); info.{field} = 0; std::hint::black_box(info);"), &["E0616", field, "private"]));
    }
    negatives.push(Case::negative("assigner_outlives_table", source.into(), "let current; { let info = CCInfo::new(1, 2, 3); current = CCAssigner::new(&info, TestPolicy::default()); } std::hint::black_box(current.get_ccinfo());", &["E0597", "info", "does not live long enough"]));
    negatives.push(Case::negative("retarget_outlives_table", source.into(), "let first = CCInfo::new(1, 2, 3); let mut current = CCAssigner::new(&first, TestPolicy::default()); { let second = CCInfo::new(4, 5, 6); current.ccinfo = &second; } std::hint::black_box(current.get_ccinfo());", &["E0597", "second", "does not live long enough"]));
    negatives.push(Case::negative("replace_borrowed_table", source.into(), "let mut info = CCInfo::new(1, 2, 3); let current = CCAssigner::new(&info, TestPolicy::default()); info = CCInfo::new(4, 5, 6); std::hint::black_box(current.get_ccinfo());", &["E0506", "info", "borrowed"]));
    negatives.push(Case::negative("mutably_replace_borrowed_table", source.into(), "let mut info = CCInfo::new(1, 2, 3); let current = CCAssigner::new(&info, TestPolicy::default()); let _ = core::mem::replace(&mut info, CCInfo::new(4, 5, 6)); std::hint::black_box(current.get_ccinfo());", &["E0502", "info", "mutable"]));
    negatives.push(Case::negative("escape_current_assigner_callback", source.into(), "let info = CCInfo::new(1, 2, 3); let mut compiler = Fixture::new(&info); let mut escaped: Option<&mut CCAssigner<'_, TestPolicy>> = None; compiler.with_cur_cc_assigner(|_, current| { escaped = Some(current); }); std::hint::black_box(escaped);", &["E0521", "borrowed data escapes", "current"]));
    negatives.push(Case::negative("escape_compiler_callback", source.into(), "let info = CCInfo::new(1, 2, 3); let mut compiler = Fixture::new(&info); let mut escaped: Option<&mut Fixture<'_>> = None; compiler.with_cur_cc_assigner(|owner, _| { escaped = Some(owner); }); std::hint::black_box(escaped);", &["E0521", "borrowed data escapes", "owner"]));

    for name in ["count", "size_bytes", "reg_bank"] {
        negatives.push(Case::negative(
            &format!("missing_parts_{name}"),
            replace_item(source, PARTS, "fn", name, ""),
            ADMIT,
            &["E0046", "not all trait items implemented", name],
        ));
    }
    for (name, declaration) in [
        ("count", "fn count(&self) -> u64 { 0 }"),
        ("size_bytes", "fn size_bytes(&self, _: u32) -> u64 { 0 }"),
        ("reg_bank", "fn reg_bank(&self, _: u32) -> u32 { 0 }"),
    ] {
        negatives.push(Case::negative(
            &format!("wrong_parts_{name}"),
            replace_item(source, PARTS, "fn", name, declaration),
            ADMIT,
            &["E0053", "incompatible type for trait", name],
        ));
    }
    negatives.push(Case::negative(
        "special_owned_payload",
        source.into(),
        "let _ = ValRefSpecial::new(4, String::new());",
        &["E0277", "Copy", "String"],
    ));
    negatives.push(Case::negative(
        "special_wrong_mode_type",
        source.into(),
        "let _ = ValRefSpecial::new(4u32, 0u64);",
        &["E0308", "expected `u8`", "found `u32`"],
    ));
    negatives.push(Case::negative("unsealed_special_layout", source.into(), "#[derive(Clone, Copy)] struct Fake { mode: u8 } impl tpde_core::compiler::ValRefSpecialStruct for Fake { fn mode(&self) -> u8 { self.mode } fn mode_mut(&mut self) -> &mut u8 { &mut self.mode } }", &["E0277", "Special", "Fake"]));

    negatives.push(Case::negative(
        "missing_fixed_assignments",
        replace_item(source, COMPILER, "const", "NUM_FIXED_ASSIGNMENTS", ""),
        ADMIT,
        &[
            "E0046",
            "not all trait items implemented",
            "NUM_FIXED_ASSIGNMENTS",
        ],
    ));
    for (name, declaration) in [
        ("width", "const NUM_FIXED_ASSIGNMENTS: [u32; 1] = [5];"),
        ("element", "const NUM_FIXED_ASSIGNMENTS: [u64; 2] = [5, 8];"),
        (
            "slice",
            "const NUM_FIXED_ASSIGNMENTS: &'static [u32] = &[5, 8];",
        ),
        (
            "container",
            "const NUM_FIXED_ASSIGNMENTS: Vec<u32> = Vec::new();",
        ),
    ] {
        negatives.push(Case::negative(
            &format!("wrong_fixed_assignments_{name}"),
            replace_item(
                source,
                COMPILER,
                "const",
                "NUM_FIXED_ASSIGNMENTS",
                declaration,
            ),
            ADMIT,
            &[
                "E0326",
                "incompatible type for trait",
                "NUM_FIXED_ASSIGNMENTS",
            ],
        ));
    }
    for name in [
        "IRValueRef",
        "IRFuncRef",
        "IRInstRef",
        "InstRange",
        "GenericValuePart",
        "ValuePart",
        "Parts",
        "ValRefSpecial",
        "CCAssigner",
    ] {
        negatives.push(Case::negative(
            &format!("missing_compiler_type_{name}"),
            replace_item(source, COMPILER, "type", name, ""),
            ADMIT,
            &["E0046", "not all trait items implemented", name],
        ));
    }
    for (name, requirement) in [
        ("Parts", "ValueParts"),
        ("ValRefSpecial", "ValRefSpecialStruct"),
        ("CCAssigner", "CallingConventionAssigner"),
    ] {
        negatives.push(Case::negative(
            &format!("invalid_compiler_type_{name}"),
            replace_item(
                source,
                COMPILER,
                "type",
                name,
                &format!("type {name} = ();"),
            ),
            ADMIT,
            &["E0277", requirement],
        ));
    }
    for name in [
        "start_func",
        "prologue_assign_arg",
        "finish_func",
        "spill_reg",
        "load_from_stack",
        "mov",
        "gval_expr_as_reg",
        "select_fixed_assignment_reg",
        "cur_func_may_emit_calls",
        "cur_personality_func",
        "cur_cc_assigner",
        "with_cur_cc_assigner",
        "try_force_fixed_assignment",
        "val_parts",
        "val_ref_special",
        "val_part_ref_special",
        "define_func_idx",
        "compile_inst",
    ] {
        negatives.push(Case::negative(
            &format!("missing_compiler_{name}"),
            replace_item(source, COMPILER, "fn", name, ""),
            ADMIT,
            &["E0046", "not all trait items implemented", name],
        ));
    }
    for (name, declaration) in [
        ("gval_expr_as_reg", "fn gval_expr_as_reg(&mut self, _: &mut u64) -> u64 { 0 }"),
        ("select_fixed_assignment_reg", "fn select_fixed_assignment_reg(&mut self, _: AssignmentPartRef<'_, '_, '_>, _: u32) -> u64 { 0 }"),
        ("cur_func_may_emit_calls", "fn cur_func_may_emit_calls(&self) -> u8 { 0 }"),
        ("cur_personality_func", "fn cur_personality_func(&self) -> u32 { 0 }"),
        ("cur_cc_assigner", "fn cur_cc_assigner(&mut self) -> &Self::CCAssigner { panic!() }"),
        ("try_force_fixed_assignment", "fn try_force_fixed_assignment(&mut self, _: u32) -> u8 { 0 }"),
        ("val_parts", "fn val_parts(&self, _: u32) -> u32 { 0 }"),
        ("val_ref_special", "fn val_ref_special(&mut self, _: u32) -> Self::ValRefSpecial { ValRefSpecial::new(4, 0) }"),
        ("val_part_ref_special", "fn val_part_ref_special(&mut self, _: &mut Self::ValRefSpecial, _: u32) -> u32 { 0 }"),
        ("compile_inst", "fn compile_inst(&mut self, _: u32, _: Self::InstRange) -> u8 { 0 }"),
    ] {
        negatives.push(Case::negative(&format!("wrong_compiler_result_{name}"), replace_item(source, COMPILER, "fn", name, declaration), ADMIT, &["E0053", "incompatible type for trait", name]));
    }
    for (label, name, declaration) in [
        (
            "generic_part",
            "gval_expr_as_reg",
            "fn gval_expr_as_reg(&mut self, _: &u64) -> Reg { Reg::new(0) }",
        ),
        (
            "special_part",
            "val_part_ref_special",
            "fn val_part_ref_special(&mut self, _: &Self::ValRefSpecial, _: u32) -> u64 { 0 }",
        ),
        (
            "instruction_range",
            "compile_inst",
            "fn compile_inst(&mut self, _: u32, _: u32) -> bool { false }",
        ),
        (
            "ir_value",
            "try_force_fixed_assignment",
            "fn try_force_fixed_assignment(&mut self, _: u64) -> bool { false }",
        ),
        (
            "ir_function",
            "define_func_idx",
            "fn define_func_idx(&mut self, _: u64, _: u32) {}",
        ),
        (
            "ir_instruction",
            "compile_inst",
            "fn compile_inst(&mut self, _: u64, _: Self::InstRange) -> bool { false }",
        ),
        (
            "assembler_register",
            "mov",
            "fn mov(&mut self, _: u64, _: Reg, _: u32) {}",
        ),
    ] {
        negatives.push(Case::negative(
            &format!("wrong_compiler_argument_{label}"),
            replace_item(source, COMPILER, "fn", name, declaration),
            ADMIT,
            &["E0053", "incompatible type for trait", name],
        ));
    }
    negatives.push(Case::negative(
        "nongeneric_prologue_assigner",
        replace_item(
            source,
            COMPILER,
            "fn",
            "prologue_assign_arg",
            "fn prologue_assign_arg(&mut self, _: &mut Self::CCAssigner, _: u32, _: u32) {}",
        ),
        ADMIT,
        &["E0049", "prologue_assign_arg", "type parameters"],
    ));
    negatives.push(Case::negative(
        "custom_without_helpers",
        custom_source(&no_custom),
        ADMIT,
        &["E0277", "CustomVariableReferenceCompiler"],
    ));
    for name in ["setup_var_ref_assignments", "load_address_of_var_reference"] {
        negatives.push(Case::negative(
            &format!("missing_custom_{name}"),
            replace_item(&custom, CUSTOM, "fn", name, ""),
            ADMIT,
            &["E0046", "not all trait items implemented", name],
        ));
    }
    negatives.push(Case::negative("wrong_custom_address_register", replace_item(&custom, CUSTOM, "fn", "load_address_of_var_reference", "fn load_address_of_var_reference(&mut self, _: u64, _: AssignmentPartRef<'_, '_, '_>) {}"), ADMIT, &["E0053", "incompatible type for trait", "load_address_of_var_reference"]));

    assert_eq!(positives.len(), 13, "complete positive control roster");
    assert_eq!(negatives.len(), 111, "complete negative requirement roster");
    let work = std::env::temp_dir().join(format!(
        "tpde-compiler-contract-refusals-{}",
        std::process::id()
    ));
    fs::create_dir_all(&work).expect("create contract fixture directory");
    let required = ["tpde_core", "tpde_assembler"];
    let deps = dependencies::current_dependency_directory(&work, &required);
    let mut libraries: Vec<(String, PathBuf)> = Vec::new();
    for name in required {
        let path = dependencies::library(&deps, name);
        if let Some(metadata) = dependencies::companion_metadata(&path) {
            libraries.push((name.to_owned(), metadata));
        }
        libraries.push((name.to_owned(), path));
    }
    verify(&work, &deps, &libraries, &positives);
    verify(&work, &deps, &libraries, &negatives);
    fs::remove_dir_all(&work).expect("remove successful contract fixtures");
    println!(
        "TPDE_COMPILER_CONTRACT_REFUSALS PASS positive_controls={} negative_cases={}",
        positives.len(),
        negatives.len()
    );
}

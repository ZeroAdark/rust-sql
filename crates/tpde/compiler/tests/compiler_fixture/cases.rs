// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use std::fmt::Write;

pub struct Program {
    pub name: String,
    pub source: String,
    pub stride: u64,
    pub tables: bool,
    pub accepted: bool,
    pub required: &'static [&'static str],
    pub forbidden: &'static [&'static str],
}

impl Program {
    fn new(name: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            source: source.into(),
            stride: 1,
            tables: true,
            accepted: true,
            required: &[],
            forbidden: &[],
        }
    }
}

pub fn programs() -> Vec<Program> {
    let mut programs = Vec::new();
    macro_rules! fixture {
        ($name:literal) => {
            programs.push(Program::new(
                $name,
                include_str!(concat!("../reference/programs/", $name, ".ir")),
            ));
        };
    }
    fixture!("abi_flags");
    fixture!("allocas");
    fixture!("arithmetic");
    fixture!("calls");
    fixture!("diamond");
    fixture!("loop");
    fixture!("multipart");
    fixture!("phi_cycle");
    fixture!("switch");
    fixture!("terminate");
    fixture!("vector");
    for (name, source) in [
        ("empty_module", ""),
        ("declarations_only", "external(%a)!\nother()!\n"),
        ("trap", "f() {\nentry:\ntrap\n}\n"),
        ("return_argument", "f(%a) {\nentry:\nret %a\n}\n"),
        ("unused_arguments", "f(%a,%b,%c,%d,%e,%f) {\nentry:\nret %c\n}\n"),
        ("no_operand_result", "f() {\nentry:\n%x = any\nret %x\n}\n"),
        ("dead_result", "f(%a) {\nentry:\n%unused = any %a\nret %a\n}\n"),
        ("repeated_operand", "f(%a) {\nentry:\n%x = any %a, %a, %a, %a\nret %x\n}\n"),
        ("conditional_select", "f(%a,%b,%c,%d) {\nentry:\n%x = condselect %a, %b, %c, %d, 1\nret %x\n}\n"),
        ("zerofill", "f() {\nentry:\n%x = zerofill 4294967295\nret %x\n}\n"),
        ("branch", "f(%a) {\nentry:\nbr ^exit\nexit:\nret %a\n}\n"),
        ("same_conditional_successor", "f(%a) {\nentry:\ncondbr %a, ^exit, ^exit\nexit:\nret %a\n}\n"),
        ("bit_test", "f(%a) {\nentry:\ntbz %a, ^yes, ^no, 63\nyes:\nret %a\nno:\ntrap\n}\n"),
        ("zero_argument_call", "f() {\nentry:\n%x = call @g\nret %x\n}\ng()!\n"),
        ("unused_call_result", "f(%a) {\nentry:\n%x = call @g, %a\nret %a\n}\ng(%a)!\n"),
        ("repeated_calls", "f(%a,%b) {\nentry:\n%x = call @g, %a, %b\n%y = call @g, %b, %x\n%z = add %y, %a\nret %z\n}\ng(%a,%b)!\n"),
        ("vector_call", "f(%v_a,%v_b,%v_c,%v_d,%v_e) {\nentry:\n%v_x = call @g, %v_a, %v_b, %v_c, %v_d, %v_e\nret %v_x\n}\ng(%v_a,%v_b,%v_c,%v_d,%v_e)!\n"),
        ("cross_bank_results", "f(%a,%v_b) {\nentry:\n%v_x = any %a\n%x = any %v_b, %v_x\nret %x\n}\n"),
        ("multipart_return", "f(%p3_a) {\nentry:\n%p3_x = any %p3_a\nret %p3_x\n}\n"),
        ("multipart_forced_request_ignored", "f(%p2_a,%p2_b) {\nentry:\n%p2_x! = any %p2_a, %p2_b\nret %p2_x\n}\n"),
        ("single_part_allow_split", "f(%split_a,%b,%c,%d,%e) {\nentry:\n%x = call @g, %b, %c, %d, %split_a, %e\nret %x\n}\ng(%a,%b,%c,%d,%e)!\n"),
        ("extended_stack_arguments", "f(%a,%b,%c,%d,%sext_e,%zext_f) {\nentry:\n%x = call @g, %a, %b, %c, %d, %sext_e, %zext_f\nret %x\n}\ng(%a,%b,%c,%d,%e,%f)!\n"),
        ("unused_alloca", "f(%a) {\nentry:\n%p = alloca 17, 16\nret %a\n}\n"),
        ("static_alloca_zero", "f() {\nentry:\n%p = alloca 0, 1\nret %p\n}\n"),
        ("static_alloca_boundary", "f() {\nentry:\n%p = alloca 256, 16\nret %p\n}\n"),
        ("dynamic_alloca_size_boundary", "f() {\nentry:\n%p = alloca 257, 16\nret %p\n}\n"),
        ("dynamic_alloca_alignment_boundary", "f() {\nentry:\n%p = alloca 8, 32\nret %p\n}\n"),
        ("variable_reference_across_call", "f() {\nentry:\n%p = alloca 8, 8\n%x = call @g, %p\n%y = any %x, %p\nret %y\n}\ng(%a)!\n"),
        ("local_function_and_declaration", "main(%a) {\nentry:\n%x = call @localfn, %a\n%y = call @external, %x\nret %y\n}\nlocalfn(%b) local {\nbody:\nret %b\n}\nexternal(%c)!\n"),
        ("module_reuses_argument_and_block_state", "first(%a,%b) {\nentry:\n%x = add %a, %b\nret %x\n}\nsecond(%v_a) {\nentry:\nret %v_a\n}\nthird() {\nentry:\nterminate\n}\n"),
        ("nested_loops", "f(%a) {\nentry:\nbr ^outer\nouter:\nbr ^inner\ninner:\ncondbr %a, ^inner, ^latch\nlatch:\ncondbr %a, ^outer, ^exit\nexit:\nret %a\n}\n"),
        ("irreducible_cfg", "f(%a) {\nentry:\ncondbr %a, ^left, ^right\nleft:\nbr ^right\nright:\ncondbr %a, ^left, ^exit\nexit:\nret %a\n}\n"),
        ("unreachable_block", "f(%a) {\nentry:\nret %a\nunreachable:\n%x = any\nret %x\n}\n"),
        ("phi_same_incoming", "f(%a,%condition) {\nentry:\ncondbr %condition, ^left, ^right\nleft:\nbr ^merge\nright:\nbr ^merge\nmerge:\n%x = phi [^left, %a], [^right, %a]\nret %x\n}\n"),
        ("multipart_phi", "f(%p2_a,%p2_b,%condition) {\nentry:\ncondbr %condition, ^left, ^right\nleft:\nbr ^merge\nright:\nbr ^merge\nmerge:\n%p2_x = phi [^left, %p2_a], [^right, %p2_b]\nret %p2_x\n}\n"),
        ("critical_edge_phi", "f(%a,%b,%condition) {\nentry:\ncondbr %condition, ^merge, ^other\nother:\n%x = add %a, %b\nbr ^merge\nmerge:\n%p! = phi [^entry, %a], [^other, %x]\nret %p\n}\n"),
        ("critical_edge_phi_reversed", "f(%a,%b,%condition) {\nentry:\ncondbr %condition, ^other, ^merge\nother:\n%x = add %a, %b\nbr ^merge\nmerge:\n%p! = phi [^entry, %a], [^other, %x]\nret %p\n}\n"),
        ("fixed_banks_saturation", "f(%a!,%b!,%c!,%d!,%e!,%v_a!,%v_b!,%v_c!,%v_d!,%v_e!) {\nentry:\n%x = any %a, %b, %c, %d, %e, %v_a, %v_b, %v_c, %v_d, %v_e\nret %x\n}\n"),
        ("forced_scalar_phi_cycle_three", "f(%a,%b,%c,%condition) {\nentry:\nbr ^loop\nloop:\n%x! = phi [^entry, %a], [^loop, %z]\n%y! = phi [^entry, %b], [^loop, %x]\n%z! = phi [^entry, %c], [^loop, %y]\ncondbr %condition, ^loop, ^exit\nexit:\n%result = any %x, %y, %z\nret %result\n}\n"),
    ] {
        programs.push(Program::new(name, source));
    }
    for count in [8, 16, 17, 24, 33] {
        let mut source = String::from("f(");
        for index in 0..count {
            write!(source, "{}%a{index}", if index == 0 { "" } else { "," }).unwrap();
        }
        source.push_str(") {\nentry:\n%x1 = add %a0, %a1\n");
        for index in 2..count {
            writeln!(source, "%x{index} = add %x{}, %a{index}", index - 1).unwrap();
        }
        writeln!(source, "ret %x{}\n}}", count - 1).unwrap();
        programs.push(Program::new(
            format!("live_argument_pressure_{count}"),
            source,
        ));
    }
    for count in [1, 31, 32, 33, 63, 64, 65, 129] {
        let mut source = String::from("f(%seed) {\nentry:\n%x0 = any %seed\n");
        for index in 1..count {
            writeln!(source, "%x{index} = any %x{}", index - 1).unwrap();
        }
        writeln!(source, "ret %x{}\n}}", count - 1).unwrap();
        programs.push(Program::new(format!("linear_results_{count}"), source));
    }
    for count in [17, 33] {
        let mut source = String::from("f(%seed) {\nentry:\n");
        for index in 0..count {
            writeln!(source, "%x{index} = any %seed").unwrap();
        }
        source.push_str("%sum1 = add %x0, %x1\n");
        for index in 2..count {
            writeln!(source, "%sum{index} = add %sum{}, %x{index}", index - 1).unwrap();
        }
        writeln!(source, "ret %sum{}\n}}", count - 1).unwrap();
        programs.push(Program::new(
            format!("live_result_pressure_{count}"),
            source,
        ));
    }
    for parts in [1, 2, 3] {
        for count in [1, 2, 3] {
            let prefix = if parts == 1 {
                ""
            } else if parts == 2 {
                "p2_"
            } else {
                "p3_"
            };
            let mut source = String::from("f(%condition");
            for index in 0..count {
                write!(source, ",%{prefix}a{index}").unwrap();
            }
            source.push_str(") {\nentry:\nbr ^loop\nloop:\n");
            for index in 0..count {
                writeln!(
                    source,
                    "%{prefix}x{index} = phi [^entry, %{prefix}a{index}], [^loop, %{prefix}x{}]",
                    (index + 1) % count
                )
                .unwrap();
            }
            source.push_str("condbr %condition, ^loop, ^exit\nexit:\n%result = any ");
            for index in 0..count {
                write!(
                    source,
                    "{}%{prefix}x{index}",
                    if index == 0 { "" } else { ", " }
                )
                .unwrap();
            }
            source.push_str("\nret %result\n}\n");
            programs.push(Program::new(
                format!("phi_cycle_parts_{parts}_length_{count}"),
                source,
            ));
        }
    }
    for (count, stride, tables) in [
        (0, 1, true),
        (1, 1, true),
        (4, 1, true),
        (5, 1, true),
        (5, 1, false),
        (5, 2, true),
        (5, 8, true),
        (5, 9, true),
        (5, 10, true),
        (9, 1, true),
        (9, 8, true),
        (9, 9, true),
        (16, 1, true),
        (16, 8, true),
        (65, 1, true),
        (65, 9, true),
    ] {
        let mut program = Program::new(
            format!(
                "switch_cases_{count}_stride_{stride}_tables_{}",
                u8::from(tables)
            ),
            switch(count, false),
        );
        program.stride = stride;
        program.tables = tables;
        if count <= 4 {
            program.forbidden = &["switch.table", "switch.binary"];
            if count > 0 {
                program.required = &["switch.equal"];
            }
        } else if ((count - 1) as u64 * stride + 1) / (count as u64) < 8 {
            program.required = if tables {
                &["switch.table", "table"]
            } else {
                &["switch.table", "switch.binary"]
            };
        } else {
            program.required = &["switch.binary"];
        }
        programs.push(program);
    }
    for count in [1, 5, 9] {
        programs.push(Program::new(
            format!("switch_phi_successors_{count}"),
            switch(count, true),
        ));
    }
    for (name, source) in [
        (
            "reject_unknown_operation",
            "f() {\nentry:\n%x = unknown\nret %x\n}\n",
        ),
        (
            "reject_missing_successor",
            "f() {\nentry:\nbr ^missing\n}\n",
        ),
        (
            "reject_missing_operand",
            "f(%a) {\nentry:\n%x = add %a\nret %x\n}\n",
        ),
        (
            "reject_wrong_call_arity",
            "f() {\nentry:\n%x = call @g\nret %x\n}\ng(%a)!\n",
        ),
        (
            "reject_immediate_overflow",
            "f() {\nentry:\n%x = alloca 4294967296, 16\nret %x\n}\n",
        ),
        ("reject_duplicate_function", "f()!\nf()!\n"),
    ] {
        let mut program = Program::new(name, source);
        program.accepted = false;
        programs.push(program);
    }
    for program in &mut programs {
        match program.name.as_str() {
            "allocas" | "dynamic_alloca_size_boundary" | "dynamic_alloca_alignment_boundary" => {
                program.required = &["dynamic.alloca"];
            }
            "unused_alloca" | "static_alloca_zero" | "static_alloca_boundary" => {
                program.forbidden = &["dynamic.alloca"];
            }
            _ => {}
        }
    }
    programs
}

fn switch(count: usize, phis: bool) -> String {
    let mut source = String::from(if phis {
        "f(%seed) {\nentry:\njump ^fallback"
    } else {
        "f() {\nentry:\njump ^fallback"
    });
    for index in 0..count {
        write!(source, ", ^case{index}").unwrap();
    }
    source.push_str(if phis {
        "\nfallback:\n%pfallback = phi [^entry, %seed]\nret %pfallback\n"
    } else {
        "\nfallback:\nterminate\n"
    });
    for index in 0..count {
        writeln!(source, "case{index}:").unwrap();
        if phis {
            writeln!(source, "%p{index} = phi [^entry, %seed]\nret %p{index}").unwrap();
        } else {
            source.push_str("terminate\n");
        }
    }
    source.push_str("}\n");
    source
}

pub fn frames(negative: bool) -> Vec<(String, String)> {
    let mut cases = vec![("zero".into(), "alloc 0\nfree 0 0\n".into())];
    for (size, rounded) in [
        (1, 1),
        (2, 2),
        (3, 4),
        (4, 4),
        (5, 8),
        (7, 8),
        (8, 8),
        (9, 16),
        (15, 16),
        (16, 16),
        (17, 32),
        (24, 32),
        (31, 32),
        (32, 32),
        (33, 48),
        (47, 48),
        (48, 48),
        (63, 64),
        (64, 64),
        (65, 80),
        (255, 256),
        (256, 256),
    ] {
        let first = if negative { -(16 + rounded) } else { 16 };
        let second = if negative {
            -(16 + 2 * rounded)
        } else {
            16 + rounded
        };
        cases.push((format!("size_{size}_lifo_reuse"), format!("alloc {size}\nalloc {size}\nfree {first} {size}\nfree {second} {size}\nalloc {size}\nalloc {size}\n")));
    }
    cases.push(("alignment_gap_classes".into(), "alloc 1\nalloc 16\nalloc 1\nalloc 2\nalloc 4\nalloc 8\nalloc 17\nalloc 3\nalloc 5\nalloc 9\n".into()));
    cases
}

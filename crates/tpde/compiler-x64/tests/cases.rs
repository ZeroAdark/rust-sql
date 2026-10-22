// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use std::fmt::Write;

pub const CASE_COUNT: usize = 121;
pub const UPSTREAM_DEFECT_CONTROLS: [&str; 4] = [
    "index_without_base_consumed_displacement",
    "pure_displacement_initializes_destination",
    "aliased_expression_negative_one_scale",
    "bitfield_insert_self_alias",
];

pub struct Case {
    pub name: String,
    pub input: String,
}

fn push(cases: &mut Vec<Case>, name: impl Into<String>, input: String) {
    cases.push(Case {
        name: name.into(),
        input,
    });
}

fn function(features: u32, body: &str) -> String {
    format!("init {features} 0 0 1\nbegin\n{body}epilog\nfinish\ndump\n")
}

fn configured(features: u32, vararg: bool, dynamic: bool, leaf: bool, body: &str) -> String {
    format!(
        "init {features} {} {} {}\nbegin\n{body}epilog\nfinish\ndump\n",
        u8::from(vararg),
        u8::from(dynamic),
        u8::from(leaf)
    )
}

pub fn high_registers() -> Case {
    let mut body = String::new();
    for high in [48, 55, 56, 63] {
        for other in [32, 47, 48, 63] {
            for size in [4, 8, 16, 32, 64] {
                writeln!(body, "mov {high} {other} {size}").unwrap();
                writeln!(body, "mov {other} {high} {size}").unwrap();
            }
        }
        for gp in [0, 7, 8, 15] {
            for size in [1, 2, 4, 8] {
                writeln!(body, "mov {high} {gp} {size}").unwrap();
                writeln!(body, "mov {gp} {high} {size}").unwrap();
            }
        }
    }
    Case {
        name: "high_vector_registers".to_owned(),
        input: function(16384 | 64, &body),
    }
}

pub fn cases() -> Vec<Case> {
    let mut out = Vec::new();
    for vararg in [0, 1] {
        let mut text = format!("ccinit {vararg}\nccinfo\n");
        for bank in [0, 1] {
            for _ in 0..10 {
                writeln!(text, "ccarg {bank} 8 8 0 0 0").unwrap();
            }
        }
        text.push_str("ccret 0 8 8 0 0 0\nccret 0 8 8 0 0 0\nccret 1 8 8 0 0 0\nccret 1 8 8 0 0 0\nccreset\nccinfo\nccarg 0 8 8 0 0 0\nccret 0 8 8 0 0 0\n");
        push(&mut out, format!("cc_exhaustion_reset_{vararg}"), text);
    }
    for occupied in 0..=6 {
        let mut text = "ccinit 0\n".to_owned();
        for _ in 0..occupied {
            text.push_str("ccarg 0 8 8 0 0 0\n");
        }
        text.push_str(
            "ccarg 0 8 8 1 0 0\nccarg 0 8 8 0 0 0\nccarg 1 4 4 0 0 0\nccarg 0 8 8 0 0 0\nccinfo\n",
        );
        push(&mut out, format!("cc_multipart_gp_{occupied}"), text);
    }
    for occupied in 0..=8 {
        let mut text = "ccinit 1\n".to_owned();
        for _ in 0..occupied {
            text.push_str("ccarg 1 8 8 0 0 0\n");
        }
        text.push_str("ccarg 1 16 16 1 0 0\nccarg 0 8 8 0 0 0\nccarg 1 4 4 0 0 0\nccinfo\n");
        push(&mut out, format!("cc_multipart_fp_{occupied}"), text);
    }
    for align in [1, 2, 4, 8, 16, 32, 64] {
        let mut text = "ccinit 0\n".to_owned();
        for size in [1, 3, 7, 8, 9, 15, 16, 17, 24, 33] {
            writeln!(text, "ccarg 0 {size} {align} 0 1 0").unwrap();
            writeln!(text, "ccarg 255 {size} {align} 0 0 0").unwrap();
        }
        text.push_str("ccarg 0 8 8 0 0 1\nccinfo\n");
        push(&mut out, format!("cc_stack_align_{align}"), text);
    }
    for features in [0, 64, 16384 | 64] {
        let mut body = String::new();
        for mask in [0, 1, 63, 64, 16383, 16384, 524287, 524288] {
            writeln!(body, "features {mask}").unwrap();
        }
        for dst in [0, 1, 7, 8, 15] {
            for src in [0, 3, 6, 9, 15] {
                for size in [1, 2, 4, 8] {
                    writeln!(body, "mov {dst} {src} {size}").unwrap();
                    writeln!(body, "mov {} {src} {size}", 32 + dst).unwrap();
                    writeln!(body, "mov {dst} {} {size}", 32 + src).unwrap();
                }
            }
        }
        for dst in [32, 39, 40, 47] {
            for src in [32, 38, 41, 47] {
                for size in [4, 8, 16] {
                    writeln!(body, "mov {dst} {src} {size}").unwrap();
                }
                if features & 64 != 0 {
                    writeln!(body, "mov {dst} {src} 32").unwrap();
                }
                if features & 16384 != 0 {
                    writeln!(body, "mov {dst} {src} 64").unwrap();
                }
            }
        }
        push(
            &mut out,
            format!("moves_features_{features}"),
            function(features, &body),
        );
    }
    for sign in [0, 1] {
        for same in [false, true] {
            let src = 7;
            let dst = if same { 7 } else { 10 };
            let mut body = String::new();
            for from in 1..64 {
                for to in [8, 16, 32, 64] {
                    if from < to {
                        writeln!(body, "intext {dst} {src} {sign} {from} {to}").unwrap();
                    }
                }
            }
            push(
                &mut out,
                format!("intext_sign_{sign}_same_{same}"),
                function(0, &body),
            );
        }
    }
    for insert in [false, true] {
        let mut body = String::new();
        for lsb in 0..63 {
            for width in 1..64 {
                if lsb + width <= 64 {
                    writeln!(
                        body,
                        "{} 10 7 {lsb} {width}",
                        if insert { "bfi" } else { "bfiz" }
                    )
                    .unwrap();
                }
            }
        }
        push(
            &mut out,
            format!("bitfields_insert_{insert}"),
            function(0, &body),
        );
    }
    for features in [0, 64] {
        for preserve in [0, 1] {
            let mut body = format!("flags {preserve}\n");
            for value in [
                0u64,
                1,
                127,
                128,
                255,
                65535,
                u32::MAX as u64,
                0x80000000,
                0x100000000,
                0x8000000000000000,
                u64::MAX,
            ] {
                for size in [1, 2, 4, 8] {
                    for dst in [0, 7, 8, 15] {
                        writeln!(body, "constant 0 {size} {dst} {value} 0").unwrap();
                    }
                }
                for size in [4, 8, 16] {
                    for dst in [32, 40, 47] {
                        writeln!(body, "constant 1 {size} {dst} {value} {value}").unwrap();
                    }
                }
            }
            push(
                &mut out,
                format!("constants_features_{features}_preserve_{preserve}"),
                function(features, &body),
            );
        }
    }
    let mut body = String::new();
    for cc in 0..16 {
        writeln!(body, "invert {cc}\ncond {cc}").unwrap();
        if (2..=7).contains(&cc) || (12..=15).contains(&cc) {
            writeln!(body, "swap {cc}").unwrap();
        }
        for dst in [0, 1, 7, 8, 15] {
            for zext in [0, 1] {
                writeln!(body, "set {cc} {dst} {zext}").unwrap();
            }
            writeln!(body, "mask {cc} {dst}").unwrap();
            for width in [0, 1] {
                writeln!(body, "cmov {cc} {dst} 9 {width}").unwrap();
            }
        }
    }
    push(&mut out, "conditions", function(0, &body));
    stack_cases(&mut out);
    frame_cases(&mut out);
    branch_cases(&mut out);
    expression_cases(&mut out);
    alloca_cases(&mut out);
    call_cases(&mut out);
    assignment_cases(&mut out);
    pressure_cases(&mut out);
    assert_eq!(
        out.len(),
        CASE_COUNT,
        "declared compiler-X64 roster changed"
    );
    out
}

fn stack_cases(out: &mut Vec<Case>) {
    for sign in [0, 1] {
        let mut body = "frame 32768 1 0\n".to_owned();
        for reg in [0, 1, 6, 7, 8, 15] {
            for size in [1, 2, 4, 8] {
                for off in [-32768i64, -4096, -129, -128, -16, -8] {
                    writeln!(body, "spill {reg} {} {size}", off as u64).unwrap();
                    writeln!(body, "load {reg} {} {size} {sign}", off as u64).unwrap();
                }
                for off in [16, 127, 128, 4096] {
                    writeln!(body, "load {reg} {off} {size} {sign}").unwrap();
                }
            }
        }
        body.push_str("state\n");
        push(out, format!("gp_stack_sign_{sign}"), function(0, &body));
    }
    let mut body = "frame 32768 1 0\n".to_owned();
    for reg in [32, 39, 40, 47] {
        for size in [4, 8, 16] {
            for off in [-32768i64, -4096, -144, -128, -16] {
                writeln!(body, "spill {reg} {} {size}", off as u64).unwrap();
                writeln!(body, "load {reg} {} {size} 0", off as u64).unwrap();
            }
            for off in [16, 128, 4096] {
                writeln!(body, "load {reg} {off} {size} 0").unwrap();
            }
        }
    }
    push(out, "vector_stack_widths", function(0, &body));
    let mut body = "frame 4096 1 0\n".to_owned();
    for off in [-4096i64, -129, -128, -8, 16, 128] {
        for dst in [0, 7, 8, 15] {
            writeln!(body, "stackaddr {dst} {}", off as u64).unwrap();
        }
        writeln!(body, "spillslot {}", off as u64).unwrap();
    }
    push(
        out,
        "stack_addresses_and_spill_expressions",
        function(0, &body),
    );
}

fn frame_cases(out: &mut Vec<Case>) {
    for (name, size, used, generated, saved) in [
        ("zero", 0, 0, 0, 0),
        ("redzone", 40, 1, 0, 0),
        ("redzone_boundary", 128, 1, 0, 0),
        ("redzone_exceeded", 129, 1, 0, 0),
        ("low_callee_saved", 40, 0, 0, 1 << 3),
        ("high_callee_saved", 40, 0, 0, (1 << 12) | (1 << 15)),
        ("all_callee_saved", 256, 1, 0, 0xf008),
        ("generated_call", 40, 0, 1, 0),
    ] {
        let body =
            format!("state\nframe {size} {used} {generated}\nclobber {saved}\npad 3 144\nstate\n");
        push(
            out,
            format!("frame_{name}"),
            configured(0, false, false, generated == 0, &body),
        );
    }
    for saved in [0, 0xf008] {
        for size in [40, 8192] {
            let body = format!("frame {size} 1 0\nclobber {saved}\nafixed 32 16\nstate\n");
            push(
                out,
                format!("dynamic_frame_saved_{saved}_size_{size}"),
                configured(0, false, true, true, &body),
            );
        }
    }
    for dynamic in [false, true] {
        push(
            out,
            format!("vararg_prologue_dynamic_{dynamic}"),
            configured(
                0,
                true,
                dynamic,
                false,
                "state\nclobber 4104\ngeneratecall 0 1 0\nstate\n",
            ),
        );
    }
    for (name, body) in [
        ("tail", "epilog\n"),
        ("shared", "epilog\npad 7 144\nepilog\npad 129 144\nepilog\n"),
        ("nontail", "epilog\npad 19 144\n"),
    ] {
        push(
            out,
            format!("multiple_return_{name}"),
            format!("init 0 0 0 1\nbegin\nframe 256 1 0\nclobber 61448\n{body}finish\ndump\n"),
        );
    }
}

fn branch_cases(out: &mut Vec<Case>) {
    for distance in [0, 126, 127, 512] {
        let mut body = String::new();
        for cc in 0..17 {
            let back = 2 * cc;
            let forward = back + 1;
            writeln!(
                body,
                "label\nplace {back}\npad {distance} 144\njump {cc} {back}\nlabel\njump {cc} {forward}\npad {distance} 144\nplace {forward}"
            )
            .unwrap();
        }
        push(
            out,
            format!("jumps_distance_{distance}"),
            function(0, &body),
        );
    }
    for width32 in [0, 1] {
        for binary in [false, true] {
            let mut body = "label\nlabel\n".to_owned();
            let values: &[u64] = if width32 != 0 {
                &[0, 127, 128, 0x7fff_ffff, 0x8000_0000, 0xffff_ffff]
            } else {
                &[
                    0,
                    127,
                    128,
                    (-128i64) as u64,
                    0x7fff_ffff,
                    0x8000_0000,
                    0x1_0000_0000,
                    u64::MAX,
                ]
            };
            for placed in [false, true] {
                if placed {
                    body.push_str("place 0\npad 9 144\nplace 1\n");
                }
                for value in values {
                    if binary {
                        writeln!(body, "binary 0 1 7 10 {value} {width32}").unwrap();
                    } else {
                        writeln!(body, "cmpeq 0 7 10 {value} {width32}").unwrap();
                    }
                }
            }
            push(
                out,
                format!("switch_comparisons_w32_{width32}_binary_{binary}"),
                function(0, &body),
            );
        }
    }
    for (width32, low) in [
        (1, 0u64),
        (1, 7),
        (1, 0x8000_0000),
        (0, 0),
        (0, 7),
        (0, 0x8000_0000),
        (0, 0x1_0000_0000),
        (0, u64::MAX - 3),
    ] {
        let mut body = format!(
            "label\nlabel\nlabel\nlabel\nlabel\ntable 0 7 10 {low} {} {width32} 1 2 3 4\n",
            low + 3
        );
        for label in 0..5 {
            writeln!(
                body,
                "place {label}\nconstant 0 8 0 {label} 0\nepilog\npad 3 144"
            )
            .unwrap();
        }
        push(
            out,
            format!("switch_table_w32_{width32}_low_{low}"),
            function(0, &body),
        );
    }
}

fn expression_cases(out: &mut Vec<Case>) {
    let displacements = [0i64, 7, -128, 0x7fff_ffff, 0x8000_0000, -0x8000_0001];
    for (name, base_kind, index_kind, before, after) in [
        (
            "borrowed",
            1,
            1,
            "hold 7\nhold 8\n",
            "release 8\nrelease 7\n",
        ),
        ("owned_base", 2, 1, "hold 8\n", "release 8\n"),
        ("owned_index", 1, 2, "hold 7\n", "release 7\n"),
        ("owned_both", 2, 2, "", ""),
    ] {
        let mut body = before.to_owned();
        for scale in [1i64, 2, 4, 8, 16, 32, 3, 15, -1, 0x1_0000_0001] {
            for disp in displacements {
                writeln!(
                    body,
                    "expr {base_kind} 7 {index_kind} 8 {} {}",
                    scale as u64, disp as u64
                )
                .unwrap();
            }
        }
        body.push_str(after);
        body.push_str("state\n");
        push(out, format!("expression_{name}"), function(0, &body));
    }
    for kind in [1, 2] {
        let mut body = if kind == 1 {
            "hold 7\n".to_owned()
        } else {
            String::new()
        };
        for preserve in [0, 1] {
            writeln!(body, "flags {preserve}").unwrap();
            for disp in displacements {
                writeln!(body, "expr {kind} 7 0 255 0 {}", disp as u64).unwrap();
            }
        }
        if kind == 1 {
            body.push_str("release 7\n");
        }
        push(
            out,
            format!("expression_base_only_kind_{kind}"),
            function(0, &body),
        );
    }
    for kind in [1, 2] {
        let mut body = if kind == 1 {
            "hold 8\n".to_owned()
        } else {
            String::new()
        };
        for scale in [2u64, 4, 8, 16, 3, 0x1_0000_0001] {
            for disp in [0, 0x1_0000_0000u64] {
                writeln!(body, "expr 0 255 {kind} 8 {scale} {disp}").unwrap();
            }
        }
        if kind == 1 {
            body.push_str("release 8\n");
        }
        push(
            out,
            format!("expression_index_only_kind_{kind}"),
            function(0, &body),
        );
    }
    for (name, base_kind, index_kind, before, after) in [
        ("borrowed", 1, 1, "hold 7\n", "release 7\n"),
        ("owned_base", 2, 1, "", ""),
        ("owned_index", 1, 2, "", ""),
    ] {
        let mut body = before.to_owned();
        for scale in [1i64, 2, 3, 7, 15, 31, -2, -3, 0x1_0000_0001] {
            for disp in [0i64, 7, 0x1_0000_0000] {
                writeln!(
                    body,
                    "expr {base_kind} 7 {index_kind} 7 {} {}",
                    scale as u64, disp as u64
                )
                .unwrap();
            }
        }
        body.push_str(after);
        push(out, format!("expression_alias_{name}"), function(0, &body));
    }
    let mut body = "flags 1\nhold 7\nhold 8\n".to_owned();
    for scale in [1, 2, 4, 8] {
        for disp in displacements {
            writeln!(body, "expr 1 7 1 8 {scale} {}", disp as u64).unwrap();
        }
    }
    body.push_str("release 8\nrelease 7\nstate\n");
    push(out, "expression_preserves_flags", function(0, &body));
}

fn alloca_cases(out: &mut Vec<Case>) {
    let mut body = String::new();
    for align in [1, 8, 16, 32, 4096] {
        for size in [0u64, 1, 15, 16, 17, 127, 128, 4096, 0x7fff_ffe0] {
            writeln!(body, "afixed {size} {align}").unwrap();
        }
    }
    body.push_str("state\n");
    push(
        out,
        "alloca_fixed_sizes_alignments",
        configured(0, false, true, true, &body),
    );
    for reg in [255, 11] {
        let mut body = String::new();
        for element in [
            0u64,
            1,
            2,
            4,
            8,
            16,
            32,
            3,
            127,
            0x7fff_ffff,
            0x8000_0001,
            u64::MAX,
        ] {
            for count in [0u64, 1, 17, 0x1_0000_0001] {
                for align in [1, 16, 64] {
                    writeln!(body, "adynamic {element} {count} {align} {reg}").unwrap();
                }
            }
        }
        push(
            out,
            format!("alloca_dynamic_count_reg_{reg}"),
            configured(0, false, true, true, &body),
        );
    }
    let mut body = String::new();
    for reg in [255, 11] {
        for element in [0u64, 1, 2, 4, 8, 16, 3, 0x7fff_ffff, 0x8000_0001, u64::MAX] {
            for align in [16, 64] {
                writeln!(body, "adynamic_ref {element} 17 {align} {reg}").unwrap();
            }
        }
    }
    push(
        out,
        "alloca_dynamic_borrowed_count",
        configured(0, false, true, true, &body),
    );
}

fn call_cases(out: &mut Vec<Case>) {
    for dynamic in [false, true] {
        for target in 0..4 {
            let mut body = String::new();
            for size in [1, 2, 4, 8] {
                body.push_str("callbegin 0\n");
                for index in 0..10 {
                    writeln!(body, "callarg 0 {size} 8 0 0 0 0 {} 0", 17 + index).unwrap();
                }
                for index in 0..11 {
                    let size = [4, 8, 16][index % 3];
                    writeln!(
                        body,
                        "callarg 1 {size} {size} 0 0 0 0 {} {}",
                        0x1234 + index,
                        0x5678 + index
                    )
                    .unwrap();
                }
                writeln!(body, "calltarget {target} 4096").unwrap();
                body.push_str(
                    "callret 0 8\ncallret 0 8\ncallret 1 8\ncallret 1 16\ncallend\nstate\n",
                );
            }
            push(
                out,
                format!("calls_stack_dynamic_{dynamic}_target_{target}"),
                configured(0, false, dynamic, false, &body),
            );
        }
    }
    for dynamic in [false, true] {
        let mut body = String::new();
        for size in [0, 1, 2, 3, 4, 5, 7, 8, 9, 15, 16, 17, 31, 32] {
            writeln!(
                body,
                "callbegin 0\ncallarg 0 {size} 16 0 1 0 0 4096 0\ncalltarget 0 {size}\ncallend"
            )
            .unwrap();
        }
        body.push_str("state\n");
        push(
            out,
            format!("calls_byval_tail_dynamic_{dynamic}"),
            configured(0, false, dynamic, false, &body),
        );
    }
    for dynamic in [false, true] {
        for target in [0, 1] {
            let mut body = String::new();
            for vectors in [0, 1, 7, 8, 10] {
                body.push_str("callbegin 1\n");
                for index in 0..vectors {
                    writeln!(body, "callarg 1 8 8 0 0 0 0 {} 0", 0x1234 + index).unwrap();
                }
                writeln!(body, "calltarget {target} 8192\ncallend").unwrap();
            }
            body.push_str("state\n");
            push(
                out,
                format!("calls_vararg_dynamic_{dynamic}_target_{target}"),
                configured(0, false, dynamic, false, &body),
            );
        }
    }
    let mut body = String::new();
    for extension in [1, 8, 16, 31, 32, 129, 136, 144, 159, 160] {
        body.push_str("callbegin 0\n");
        for index in 0..8 {
            writeln!(
                body,
                "callarg 0 8 8 0 0 0 {extension} {} 0",
                0xfedc_ba98u64 + index
            )
            .unwrap();
        }
        body.push_str("calltarget 0 1\ncallend\n");
    }
    push(
        out,
        "calls_integer_argument_extensions",
        configured(0, false, false, false, &body),
    );
    for dynamic in [false, true] {
        let mut body = String::new();
        for target in 0..4 {
            for vararg in [0, 1] {
                writeln!(body, "generatecall {target} 4096 {vararg}\nstate").unwrap();
            }
        }
        body.push_str("reset\n");
        push(
            out,
            format!("public_generate_call_dynamic_{dynamic}"),
            configured(0, false, dynamic, false, &body),
        );
    }
    for dynamic in [false, true] {
        let mut body = "hold 3\nstate\n".to_owned();
        for model in 0..4 {
            writeln!(body, "tls {model}\nstate\ntls {model}\nstate").unwrap();
            writeln!(body, "tlslive 11 123 {model}\ntlslive 39 37 {model}\nstate").unwrap();
        }
        body.push_str("release 3\nreset\n");
        push(
            out,
            format!("tls_models_cache_dynamic_{dynamic}"),
            configured(0, false, dynamic, false, &body),
        );
    }
    let mut body = "callbegin 1\n".to_owned();
    for bank in [0, 1] {
        for index in 0..11 {
            let size = if bank == 0 { 8 } else { [4, 8, 16][index % 3] };
            writeln!(
                body,
                "callarg_loaded {bank} {size} {size} 0 0 0 0 {} {}",
                17 + index,
                34 + index
            )
            .unwrap();
        }
    }
    body.push_str("callarg 0 15 16 0 1 0 0 4096 0\ncalltarget 2 8192\ncallret 0 8\ncallret 1 8\ncallend\nstate\n");
    push(
        out,
        "call_arguments_preloaded_registers",
        configured(0, false, true, false, &body),
    );
}

fn assignment_cases(out: &mut Vec<Case>) {
    const ALL: u64 = 0x0000_ffff_0000_ffcf;
    for leaf in [false, true] {
        let mut body = String::new();
        for available in [ALL, 0xf008, 0x3c6, 0x0000_ffff_0000_0000, 0] {
            writeln!(body, "regs {available}").unwrap();
            for bank in [0, 1] {
                for exclude in [0u64, 7, 0xf008, u64::MAX] {
                    writeln!(body, "fixed {bank} {exclude}").unwrap();
                }
            }
        }
        writeln!(body, "regs {ALL}\nhold 3\nhold 6\nhold 32\nfixed 0 7\nfixed 1 0\nrelease 32\nrelease 6\nrelease 3\nstate").unwrap();
        push(
            out,
            format!("fixed_selection_leaf_{leaf}"),
            configured(0, false, false, leaf, &body),
        );
    }
    for bank in [0, 1] {
        let mut body = String::new();
        let sizes: &[u32] = if bank == 0 {
            &[1, 2, 4, 8]
        } else {
            &[4, 8, 16]
        };
        let reg = if bank == 0 { 7 } else { 39 };
        for size in sizes {
            writeln!(body, "proarg 0 {bank} {size} 1 {reg} 0 0").unwrap();
            writeln!(body, "proarg 0 {bank} {size} 1 255 32 0").unwrap();
            writeln!(body, "proarg 1 {bank} {size} 1 {reg} 0 0").unwrap();
            writeln!(body, "proarg 1 {bank} {size} 1 255 32 0").unwrap();
            for parts in [2, 3] {
                writeln!(body, "proarg 1 {bank} {size} {parts} 255 128 0").unwrap();
            }
            writeln!(body, "proarg 2 {bank} {size} 1 255 16 0").unwrap();
            writeln!(body, "proarg 2 {bank} {size} 2 255 128 0").unwrap();
        }
        if bank == 0 {
            body.push_str("proarg 1 0 8 1 255 24 1\nproarg 0 0 8 1 255 24 1\n");
        }
        body.push_str("state\n");
        push(
            out,
            format!("prologue_argument_branches_bank_{bank}"),
            function(0, &body),
        );
    }
    let mut body = String::new();
    for scalar in [0, 1, 5, 6, 7, 9] {
        for vector in [0, 1, 7, 8, 9, 10] {
            writeln!(body, "proend {scalar} {vector}\nstate").unwrap();
        }
    }
    push(
        out,
        "vararg_prologue_counts_and_incoming_stack",
        configured(0, true, false, true, &body),
    );
}

fn pressure_cases(out: &mut Vec<Case>) {
    for features in [0, 64] {
        for preserve in [0, 1] {
            let mut body = format!("flags {preserve}\nregs {}\n", 0x0000_ffff_0000_0000u64);
            for size in [4, 8, 16] {
                for dst in [32, 40, 47] {
                    for (low, high) in [
                        (0u64, 0),
                        (u64::MAX, u64::MAX),
                        (0x1234_5678, 0),
                        (0x1_0000_0000, 0),
                        (0, 0x1_0000_0001),
                        (u64::MAX, 0),
                    ] {
                        writeln!(body, "constant 1 {size} {dst} {low} {high}").unwrap();
                    }
                }
            }
            writeln!(body, "regs {}\nstate", 0x0000_ffff_0000_ffcfu64).unwrap();
            push(
                out,
                format!("constants_without_gp_features_{features}_preserve_{preserve}"),
                function(features, &body),
            );
        }
    }
}

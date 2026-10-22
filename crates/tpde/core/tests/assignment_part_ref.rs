// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![cfg(feature = "upstream-oracle")]

#[path = "../../test-support/oracle.rs"]
mod oracle;

use mcx::MemoryContext;
use std::{
    fmt::Write,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};
use tpde_core::{
    assignment_part_ref::AssignmentPartRef,
    value_assignment::{with_assignment_allocator, Assignment, AssignmentInit},
    Reg, RegBank,
};

const INDICES: [u32; 12] = [
    0,
    1,
    2,
    3,
    255,
    256,
    65535,
    65536,
    16843009,
    16843010,
    2147483648,
    u32::MAX,
];
const FRAMES: [i32; 6] = [1, -1, i32::MIN, i32::MAX, -4096, 4096];
const CONTROLS: [&str; 12] = [
    "bank",
    "reg",
    "size-power",
    "size-range",
    "frame-variable",
    "frame-zero",
    "stack-nonref",
    "stack-nonstack",
    "stack-part",
    "custom-nonref",
    "custom-stack",
    "custom-part",
];
const ROW_BYTES: usize = 140;

struct Executables {
    assertions: PathBuf,
    ndebug: PathBuf,
    sanitized: PathBuf,
}
fn executables() -> &'static Executables {
    static PATHS: OnceLock<Executables> = OnceLock::new();
    PATHS.get_or_init(|| {
        let base =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/assignment-part-ref-reference");
        let work =
            std::env::temp_dir().join(format!("tpde-assignment-part-ref-{}", std::process::id()));
        fs::create_dir_all(&work).unwrap();
        let assertions = work.join("assertions");
        let ndebug = work.join("ndebug");
        let sanitized = if cfg!(tpde_address_sanitizer) {
            assertions.clone()
        } else {
            work.join("sanitized")
        };
        let sanitized_flags = [
            "-UNDEBUG",
            "-fsanitize=address,undefined",
            "-fno-sanitize-recover=all",
        ];
        let active: &[&str] = if cfg!(tpde_address_sanitizer) {
            &sanitized_flags
        } else {
            &["-UNDEBUG"]
        };
        let sources = [
            base.join("oracle.cpp"),
            base.join("tpde/src/ValueAssignment.cpp"),
            base.join("tpde/src/util/SmallVector.cpp"),
        ];
        oracle::compile_cpp_with_flags(
            &base.join("tpde/include"),
            &assertions,
            &sources,
            true,
            active,
        );
        let ndebug_flags: &[&str] = if cfg!(tpde_address_sanitizer) {
            &["-fsanitize=address,undefined", "-fno-sanitize-recover=all"]
        } else {
            &[]
        };
        oracle::compile_cpp_with_flags(
            &base.join("tpde/include"),
            &ndebug,
            &sources,
            false,
            ndebug_flags,
        );
        if !cfg!(tpde_address_sanitizer) {
            oracle::compile_cpp_with_flags(
                &base.join("tpde/include"),
                &sanitized,
                &sources,
                true,
                &sanitized_flags,
            );
        }
        Executables {
            assertions,
            ndebug,
            sanitized,
        }
    })
}

fn init() -> AssignmentInit {
    AssignmentInit {
        frame_off: -0x1234567,
        references_left: 0x98765432,
        max_part_size: 128,
        pending_free: true,
        variable_ref: false,
        stack_variable: true,
        delay_free: true,
    }
}
fn header(value: &Assignment<'_, '_>) -> [u32; 8] {
    [
        value.var_ref_custom_idx(),
        value.part_count(),
        value.references_left(),
        value.max_part_size() as u32,
        value.pending_free() as u32,
        value.variable_ref() as u32,
        value.stack_variable() as u32,
        value.delay_free() as u32,
    ]
}
fn append16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}
fn append32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn packed_words() -> Vec<u8> {
    let context = MemoryContext::new("assignment-part-ref-oracle");
    let mut output = Vec::with_capacity(65536 * ROW_BYTES);
    with_assignment_allocator(context.mcx(), |allocator| {
        let value = allocator
            .allocate(3)
            .unwrap()
            .initialize(init(), |index| [0xc369, 0, 0x96ac][index as usize]);
        let view = AssignmentPartRef::new(&value, 1);
        let original_header = header(&value);
        for raw in 0..=u16::MAX {
            value.part(1).set(raw);
            assert!(std::ptr::eq(view.assignment(), &*value));
            for getter in [
                view.bank().id() as u32,
                view.get_reg().id() as u32,
                view.modified() as u32,
                view.fixed_assignment() as u32,
                view.register_valid() as u32,
                view.stack_valid() as u32,
                view.part_size(),
            ] {
                append32(&mut output, getter);
            }
            macro_rules! mutation {
                ($action:expr) => {{
                    value.part(1).set(raw);
                    $action;
                    assert_eq!(header(&value), original_header);
                    assert_eq!((value.part(0).get(), value.part(2).get()), (0xc369, 0x96ac));
                    assert!(std::ptr::eq(view.assignment(), &*value));
                    append16(&mut output, value.part(1).get());
                }};
            }
            mutation!(view.reset());
            for bank in 0..8 {
                mutation!(view.set_bank(RegBank::new(bank)));
            }
            for low in 0..32 {
                mutation!(view.set_reg(Reg::new(u64::from(((raw >> 5) & 7) * 32 + low))));
            }
            for flag in [false, true] {
                mutation!(view.set_modified(flag));
            }
            for flag in [false, true] {
                mutation!(view.set_fixed_assignment(flag));
            }
            for flag in [false, true] {
                mutation!(view.set_register_valid(flag));
            }
            mutation!(view.set_stack_valid());
            for shift in 0..8 {
                mutation!(view.set_part_size(1 << shift));
            }
        }
        allocator.deallocate(value);
    });
    assert_eq!(output.len(), 65536 * ROW_BYTES);
    assert_eq!(context.used(), 0);
    output
}

fn field_name(offset: usize) -> (String, usize, usize) {
    if offset < 28 {
        let index = offset / 4;
        return (
            [
                "bank",
                "get_reg",
                "modified",
                "fixed_assignment",
                "register_valid",
                "stack_valid",
                "part_size",
            ][index]
                .into(),
            index * 4,
            4,
        );
    }
    let index = (offset - 28) / 2;
    let name = match index {
        0 => "reset".into(),
        1..=8 => format!("set_bank({})", index - 1),
        9..=40 => format!("set_reg(bank*32+{})", index - 9),
        41..=42 => format!("set_modified({})", index - 41),
        43..=44 => format!("set_fixed_assignment({})", index - 43),
        45..=46 => format!("set_register_valid({})", index - 45),
        47 => "set_stack_valid".into(),
        _ => format!("set_part_size({})", 1 << (index - 48)),
    };
    (name, 28 + index * 2, 2)
}
fn compare_packed(executable: &Path, expected: &[u8]) {
    let result = Command::new(executable).arg("exhaustive").output().unwrap();
    assert!(
        result.status.success(),
        "C++ exhaustive failed {}: {}",
        result.status,
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        result.stdout.len(),
        expected.len(),
        "C++ packed-record length"
    );
    if let Some(index) = expected
        .iter()
        .zip(&result.stdout)
        .position(|(a, b)| a != b)
    {
        let raw = index / ROW_BYTES;
        let (name, field_offset, width) = field_name(index % ROW_BYTES);
        let start = raw * ROW_BYTES + field_offset;
        panic!(
            "packed word {raw:#06x}, {name}: Rust {:?}, C++ {:?}",
            &expected[start..start + width],
            &result.stdout[start..start + width]
        );
    }
}

#[test]
fn every_packed_word_getter_and_admitted_setter_matches_cpp() {
    let expected = packed_words();
    let paths = executables();
    for path in [&paths.assertions, &paths.ndebug] {
        compare_packed(path, &expected);
    }
    println!("TPDE_ASSIGNMENT_PART_REF_ORACLE PASS active={} words=65536 getters=458752 setter_results=3670016 cpp_modes=2", usize::from(cfg!(tpde_address_sanitizer)));
}

fn offsets() -> String {
    let context = MemoryContext::new("assignment-part-offsets");
    let mut output = String::new();
    with_assignment_allocator(context.mcx(), |allocator| {
        let value = allocator.allocate(3).unwrap().initialize(init(), |_| 0);
        value.set_variable_ref(false);
        value.set_stack_variable(false);
        for maximum in 0..=255u8 {
            value.set_max_part_size(maximum);
            for index in INDICES {
                for frame in FRAMES {
                    value.set_frame_off(frame);
                    let view = AssignmentPartRef::new(&value, index);
                    assert!(std::ptr::eq(view.assignment(), &*value));
                    writeln!(
                        output,
                        "O {maximum} {index} {frame} {} {}",
                        view.part_off(),
                        view.frame_off()
                    )
                    .unwrap();
                }
            }
        }
        for variable in [false, true] {
            for stack in [false, true] {
                for frame in FRAMES {
                    value.set_variable_ref(variable);
                    value.set_stack_variable(stack);
                    value.set_frame_off(frame);
                    let view = AssignmentPartRef::new(&value, 0);
                    let result = if variable {
                        if stack {
                            view.variable_stack_off() as u32
                        } else {
                            view.variable_ref_data()
                        }
                    } else {
                        view.frame_off() as u32
                    };
                    writeln!(
                        output,
                        "H {} {} {frame} {} {} {result}",
                        variable as u32,
                        stack as u32,
                        view.variable_ref() as u32,
                        view.is_stack_variable() as u32
                    )
                    .unwrap();
                }
            }
        }
        allocator.deallocate(value);
    });
    assert_eq!(context.used(), 0);
    output
}

fn reuse_snapshot(
    output: &mut String,
    epoch: u32,
    step: u32,
    value: &Assignment<'_, '_>,
    view: AssignmentPartRef<'_, '_, '_>,
) {
    assert!(std::ptr::eq(view.assignment(), &**value));
    let frame = if view.variable_ref() {
        0
    } else {
        view.frame_off() as u32
    };
    writeln!(
        output,
        "R {epoch} {step} {} {} {} {} {} {} {} {} {} {frame}",
        value.part(0).get(),
        value.part(1).get(),
        value.part(2).get(),
        value.var_ref_custom_idx(),
        value.max_part_size(),
        value.references_left(),
        view.variable_ref() as u32,
        view.is_stack_variable() as u32,
        view.part_off()
    )
    .unwrap();
}
fn reuse() -> String {
    let context = MemoryContext::new("assignment-part-reuse");
    let mut output = String::new();
    with_assignment_allocator(context.mcx(), |mut allocator| {
        let mut previous = std::ptr::null();
        for epoch in 0..3 {
            let value = allocator.allocate(3).unwrap().initialize(
                AssignmentInit {
                    frame_off: -128,
                    references_left: 17,
                    max_part_size: 16,
                    ..AssignmentInit::default()
                },
                |index| [0x1234, 0xabcd, 0x55aa][index as usize],
            );
            writeln!(output, "P {epoch} {}", (previous == value.as_ptr()) as u32).unwrap();
            previous = value.as_ptr();
            let first = AssignmentPartRef::new(&value, 1);
            let second = first;
            assert!(std::ptr::eq(first.assignment(), second.assignment()));
            reuse_snapshot(&mut output, epoch, 0, &value, second);
            first.reset();
            reuse_snapshot(&mut output, epoch, 1, &value, second);
            first.set_bank(RegBank::new(3));
            reuse_snapshot(&mut output, epoch, 2, &value, second);
            first.set_reg(Reg::new(109));
            reuse_snapshot(&mut output, epoch, 3, &value, second);
            first.set_part_size(32);
            reuse_snapshot(&mut output, epoch, 4, &value, second);
            first.set_register_valid(true);
            reuse_snapshot(&mut output, epoch, 5, &value, second);
            first.set_fixed_assignment(true);
            reuse_snapshot(&mut output, epoch, 6, &value, second);
            first.set_stack_valid();
            reuse_snapshot(&mut output, epoch, 7, &value, second);
            first.set_modified(true);
            reuse_snapshot(&mut output, epoch, 8, &value, second);
            first.set_register_valid(false);
            reuse_snapshot(&mut output, epoch, 9, &value, second);
            value.set_frame_off(i32::MAX);
            value.set_max_part_size(255);
            value.set_references_left(37);
            reuse_snapshot(&mut output, epoch, 10, &value, second);
            value.set_variable_ref(true);
            value.set_stack_variable(false);
            value.set_var_ref_custom_idx(0xf1234567);
            reuse_snapshot(&mut output, epoch, 11, &value, second);
            value.set_stack_variable(true);
            value.set_frame_off(i32::MIN);
            reuse_snapshot(&mut output, epoch, 12, &value, second);
            value.set_variable_ref(false);
            value.set_stack_variable(false);
            value.set_frame_off(1);
            reuse_snapshot(&mut output, epoch, 13, &value, second);
            allocator.deallocate(value);
            if epoch == 1 {
                allocator.reset();
            }
        }
    });
    assert_eq!(context.used(), 0);
    output
}
fn compare_lines(executable: &Path, mode: &str, expected: &str) {
    let result = Command::new(executable).arg(mode).output().unwrap();
    assert!(
        result.status.success(),
        "C++ {mode} failed {}: {}",
        result.status,
        String::from_utf8_lossy(&result.stderr)
    );
    let actual = String::from_utf8(result.stdout).unwrap();
    let mut left = expected.lines();
    let mut right = actual.lines();
    let mut line = 1;
    loop {
        let a = left.next();
        let b = right.next();
        assert_eq!(a, b, "{mode} trace line {line}");
        if a.is_none() {
            break;
        }
        line += 1;
    }
    assert!(
        expected.as_bytes() == actual.as_bytes(),
        "{mode} protocol line endings or terminal newline differ: Rust {} bytes, C++ {} bytes",
        expected.len(),
        actual.len()
    );
}
#[test]
fn offset_wrap_header_forms_and_reused_aliases_match_cpp() {
    let offsets = offsets();
    let reuse = reuse();
    let paths = executables();
    for path in [&paths.assertions, &paths.ndebug] {
        compare_lines(path, "offsets", &offsets);
        compare_lines(path, "reuse", &reuse);
    }
    println!("TPDE_ASSIGNMENT_PART_REF_OFFSETS PASS offsets=18432 header_forms=24 reuse_epochs=3 cpp_modes=2");
}

fn rust_control(name: &str, bad: bool) {
    let context = MemoryContext::new("assignment-part-control");
    with_assignment_allocator(context.mcx(), |allocator| {
        let value = allocator.allocate(3).unwrap().initialize(init(), |_| 0x41);
        let index = u32::from(bad && matches!(name, "stack-part" | "custom-part"));
        let view = AssignmentPartRef::new(&value, index);
        match name {
            "bank" => view.set_bank(RegBank::new(if bad { 8 } else { 7 })),
            "reg" => view.set_reg(Reg::new(if bad { 33 } else { 65 })),
            "size-power" => view.set_part_size(if bad { 3 } else { 4 }),
            "size-range" => view.set_part_size(if bad { 256 } else { 128 }),
            "frame-variable" => {
                value.set_variable_ref(bad);
                std::hint::black_box(view.frame_off());
            }
            "frame-zero" => {
                value.set_frame_off(if bad { 0 } else { -128 });
                std::hint::black_box(view.frame_off());
            }
            "stack-nonref" => {
                value.set_variable_ref(!bad);
                value.set_stack_variable(true);
                std::hint::black_box(view.variable_stack_off());
            }
            "stack-nonstack" => {
                value.set_variable_ref(true);
                value.set_stack_variable(!bad);
                std::hint::black_box(view.variable_stack_off());
            }
            "stack-part" => {
                value.set_variable_ref(true);
                value.set_stack_variable(true);
                std::hint::black_box(view.variable_stack_off());
            }
            "custom-nonref" => {
                value.set_variable_ref(!bad);
                value.set_stack_variable(false);
                value.set_var_ref_custom_idx(0xfeed0001);
                std::hint::black_box(view.variable_ref_data());
            }
            "custom-stack" => {
                value.set_variable_ref(true);
                value.set_stack_variable(bad);
                value.set_var_ref_custom_idx(0xfeed0001);
                std::hint::black_box(view.variable_ref_data());
            }
            "custom-part" => {
                value.set_variable_ref(true);
                value.set_stack_variable(false);
                value.set_var_ref_custom_idx(0xfeed0001);
                std::hint::black_box(view.variable_ref_data());
            }
            "zero-size" => view.set_part_size(0),
            _ => unreachable!(),
        }
        allocator.deallocate(value);
    });
}
#[test]
fn source_assertions_have_positive_controls_and_zero_ctz_is_diagnosed() {
    let paths = executables();
    for name in CONTROLS {
        rust_control(name, false);
        assert_eq!(
            std::panic::catch_unwind(|| rust_control(name, true)).is_err(),
            tpde_util::CHECKED,
            "Rust accepted {name}"
        );
        let good = Command::new(&paths.assertions)
            .args(["control", name, "good"])
            .output()
            .unwrap();
        assert!(
            good.status.success(),
            "C++ positive {name}: {}",
            String::from_utf8_lossy(&good.stderr)
        );
        assert_eq!(good.stdout, b"OK\n");
        let bad = Command::new(&paths.assertions)
            .args(["control", name, "bad"])
            .output()
            .unwrap();
        assert!(!bad.status.success(), "C++ assertion disabled for {name}");
        let diagnostic = String::from_utf8_lossy(&bad.stderr);
        assert!(
            diagnostic.contains("Assertion") && diagnostic.contains("failed"),
            "unexpected {name} failure: {diagnostic}"
        );
    }
    let positive = Command::new(&paths.sanitized)
        .args(["control", "size-range", "good"])
        .output()
        .unwrap();
    assert!(
        positive.status.success(),
        "sanitizer positive: {}",
        String::from_utf8_lossy(&positive.stderr)
    );
    let zero = Command::new(&paths.sanitized)
        .arg("zero-size")
        .output()
        .unwrap();
    assert!(
        !zero.status.success(),
        "source __builtin_ctz(0) went undiagnosed"
    );
    let diagnostic = String::from_utf8_lossy(&zero.stderr);
    assert!(
        diagnostic.contains(
            "runtime error: passing zero to __builtin_ctz(), which is not a valid argument"
        ) && diagnostic.contains("tpde/util/misc.hpp:43:24")
            && diagnostic.contains("SUMMARY: UndefinedBehaviorSanitizer"),
        "unexpected zero-size failure: {diagnostic}"
    );
    assert_eq!(
        std::panic::catch_unwind(|| rust_control("zero-size", true)).is_err(),
        tpde_util::CHECKED
    );
    println!("TPDE_ASSIGNMENT_PART_REF_CONTROLS PASS positive=13 assertions=12 source_ub=1 rust_zero_rejected=1");
}

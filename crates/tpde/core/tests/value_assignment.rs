// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![cfg(feature = "upstream-oracle")]

#[path = "../../test-support/oracle.rs"]
mod oracle;

use mcx::MemoryContext;
use std::{
    fmt::Write,
    fs, mem,
    path::Path,
    process::{Command, Stdio},
};
use tpde_base::ValLocalIdx;
use tpde_core::value_assignment::{
    with_assignment_allocator, Assignment, AssignmentAllocationInfo, AssignmentAllocator,
    AssignmentInit, ValueAssignment, FIRST_PART_OFFSET, NUM_PARTS_INCLUDED,
};
use tpde_util::BumpStats;

#[derive(Clone, Copy)]
struct Op(char, [u32; 5]);

#[derive(Default)]
struct Ids {
    regular: Vec<usize>,
    large: Vec<usize>,
    next: usize,
}
impl Ids {
    fn sync(&mut self, stats: BumpStats) {
        self.regular.truncate(stats.regular_slabs);
        self.large.truncate(stats.large_slabs);
        while self.regular.len() < stats.regular_slabs {
            self.regular.push(self.next);
            self.next += 1;
        }
        while self.large.len() < stats.large_slabs {
            self.large.push(self.next);
            self.next += 1;
        }
    }
}

fn flags(value: &ValueAssignment) -> u32 {
    u32::from(value.pending_free())
        | (u32::from(value.variable_ref()) << 1)
        | (u32::from(value.stack_variable()) << 2)
        | (u32::from(value.delay_free()) << 3)
}
fn snapshot(
    operation: char,
    slot: usize,
    value: &Assignment<'_, '_>,
    allocator: &AssignmentAllocator<'_, '_>,
    ids: &mut Ids,
    output: &mut String,
) {
    let info = allocator.allocation_info(value.as_ptr()).unwrap();
    let stats = allocator.stats();
    ids.sync(stats);
    let id = if info.large {
        ids.large[info.index]
    } else {
        ids.regular[info.index]
    };
    let checksum = value.parts().iter().fold(0u64, |sum, part| {
        sum.wrapping_mul(131).wrapping_add(u64::from(part.get()))
    });
    let size = if value.variable_ref() {
        0
    } else {
        value.size()
    };
    writeln!(
        output,
        "{operation} {slot} {id} {} {} {} {} {} {} {} {} {} {size} {checksum}",
        info.offset,
        info.size,
        stats.regular_slabs + stats.large_slabs,
        stats.allocated_bytes,
        value.var_ref_custom_idx(),
        value.part_count(),
        value.references_left(),
        value.max_part_size(),
        flags(value)
    )
    .unwrap();
}

fn epochs() -> Vec<Vec<Op>> {
    let counts = [
        0, 1, 2, 3, 8, 9, 10, 24, 25, 56, 57, 120, 121, 248, 249, 504, 505, 1016, 1017, 2040, 2041,
        4088, 4089, 8184, 8185, 16376, 16377, 32760, 32761,
    ];
    let mut first = Vec::new();
    for (slot, count) in counts.into_iter().enumerate() {
        first.push(Op(
            'A',
            [
                slot as u32,
                count,
                slot as u32 % 3,
                (slot as u32).wrapping_mul(0x1010101).wrapping_add(255),
                slot as u32 % 16,
            ],
        ));
    }
    for field in 0..9 {
        for value in [0, 1, u32::MAX, 0x80000000] {
            first.push(Op('M', [2, field, value, 0, 0]));
        }
    }
    first.extend([
        Op('M', [2, 9, 0xffff, 0, 0]),
        Op('M', [2, 10, 0x8123, 0, 0]),
    ]);
    for slot in (0..counts.len()).rev() {
        first.push(Op('F', [slot as u32, slot as u32 % 2, 0, 0, 0]));
    }
    for (slot, count) in counts.into_iter().rev().enumerate() {
        first.push(Op(
            'A',
            [slot as u32, count, 0, 0x11110000 + slot as u32, 0],
        ));
    }
    for slot in 0..counts.len() {
        first.push(Op('F', [slot as u32, 0, 0, 0, 0]));
    }
    for count in [0, 1, 2, 8, 9, 24, 25, 8185, 32761] {
        first.extend([
            Op('A', [0, count, 0, 1, 0]),
            Op('A', [1, count, 0, 2, 0]),
            Op('F', [0, 0, 0, 0, 0]),
            Op('F', [1, 1, 0, 0, 0]),
            Op('A', [2, count, 2, 3, 0]),
            Op('A', [0, count, 1, 4, 0]),
            Op('A', [1, count, 0, 5, 0]),
            Op('F', [0, 0, 0, 0, 0]),
            Op('F', [1, 0, 0, 0, 0]),
            Op('F', [2, 0, 0, 0, 0]),
        ]);
    }
    let mut growth = Vec::new();
    for slot in 0..40 {
        growth.push(Op('A', [slot, 8185, 2, slot, slot % 16]));
    }
    for slot in (0..40).rev() {
        growth.push(Op('F', [slot, slot % 2, 0, 0, 0]));
    }
    let mut random = Vec::new();
    let mut state = 0x93ab1d77u32;
    let mut live = [false; 32];
    for _ in 0..600 {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        let slot = state as usize % live.len();
        if live[slot] {
            random.push(Op('F', [slot as u32, state % 2, 0, 0, 0]));
        } else {
            random.push(Op(
                'A',
                [
                    slot as u32,
                    counts[(state >> 5) as usize % counts.len()],
                    (state >> 12) % 3,
                    state,
                    state % 16,
                ],
            ));
        }
        live[slot] = !live[slot];
    }
    for (slot, live) in live.into_iter().enumerate() {
        if live {
            random.push(Op('F', [slot as u32, 0, 0, 0, 0]));
        }
    }
    vec![
        Vec::new(),
        first,
        growth,
        random,
        vec![Op('A', [0, 1, 0, 42, 0]), Op('F', [0, 0, 0, 0, 0])],
    ]
}

fn epoch(
    allocator: &AssignmentAllocator<'_, '_>,
    ops: &[Op],
    ids: &mut Ids,
    input: &mut String,
    output: &mut String,
) {
    let mut slots: Vec<Option<Assignment<'_, '_>>> = (0..128).map(|_| None).collect();
    #[cfg(tpde_address_sanitizer)]
    let mut retired = [std::ptr::null::<u8>(); 128];
    for &Op(code, args) in ops {
        let [a, b, c, d, e] = args;
        match code {
            'A' => {
                writeln!(input, "A {a} {b} {c} {d} {e}").unwrap();
                assert!(slots[a as usize].is_none());
                let allocation = if c == 0 {
                    allocator.allocate(b)
                } else {
                    allocator.allocate_slow(b, c == 2)
                }
                .unwrap();
                if c == 2 {
                    // SAFETY: skip-free-list slow allocation initializes the entire fixed header before publication.
                    let header = unsafe { &*allocation.as_ptr() };
                    // SAFETY: the first part is inside the same initialized fixed header, even when requested count is zero.
                    let first = unsafe {
                        allocation
                            .as_ptr()
                            .cast::<u8>()
                            .add(FIRST_PART_OFFSET as usize)
                            .cast::<u16>()
                            .read()
                    };
                    writeln!(
                        output,
                        "Z {} {} {} {} {} {first}",
                        header.var_ref_custom_idx(),
                        header.part_count(),
                        header.references_left(),
                        header.max_part_size(),
                        flags(header)
                    )
                    .unwrap();
                }
                let value = allocation.initialize(
                    AssignmentInit {
                        frame_off: (d ^ 0x87654321) as i32,
                        references_left: d.rotate_left(11),
                        max_part_size: d as u8,
                        pending_free: e & 1 != 0,
                        variable_ref: e & 2 != 0,
                        stack_variable: e & 4 != 0,
                        delay_free: e & 8 != 0,
                    },
                    |index| d.wrapping_add(index.wrapping_mul(31337)) as u16,
                );
                snapshot(code, a as usize, &value, allocator, ids, output);
                slots[a as usize] = Some(value);
            }
            'M' => {
                writeln!(input, "M {a} {b} {c}").unwrap();
                let value = slots[a as usize].as_ref().unwrap();
                match b {
                    0 => value.set_frame_off(c as i32),
                    1 => value.set_var_ref_custom_idx(c),
                    2 => value.set_references_left(c),
                    3 => value.set_next_delayed_free_entry(ValLocalIdx::new(c)),
                    4 => value.set_max_part_size(c as u8),
                    5 => value.set_pending_free(c != 0),
                    6 => value.set_variable_ref(c != 0),
                    7 => value.set_stack_variable(c != 0),
                    8 => value.set_delay_free(c != 0),
                    _ => value.part(b - 9).set(c as u16),
                }
                snapshot(code, a as usize, value, allocator, ids, output);
            }
            'F' => {
                writeln!(input, "F {a} {b}").unwrap();
                let value = slots[a as usize].take().unwrap();
                #[cfg(tpde_address_sanitizer)]
                {
                    retired[a as usize] = value.as_ptr().cast();
                }
                if b == 0 {
                    allocator.deallocate(value);
                } else {
                    allocator.deallocate_slow(value);
                }
                let stats = allocator.stats();
                writeln!(
                    output,
                    "F {a} {b} {} {}",
                    stats.regular_slabs + stats.large_slabs,
                    stats.allocated_bytes
                )
                .unwrap();
            }
            #[cfg(tpde_address_sanitizer)]
            'P' => {
                extern "C" {
                    fn __asan_address_is_poisoned(pointer: *const u8) -> i32;
                }
                writeln!(input, "P {a} {b} {c}").unwrap();
                let pointer = if c == 0 {
                    slots[a as usize].as_ref().unwrap().as_ptr().cast::<u8>()
                } else {
                    retired[a as usize]
                };
                // SAFETY: shadow queries address the still-live retained allocation without reading poisoned data.
                let poisoned = unsafe { __asan_address_is_poisoned(pointer.add(b as usize)) != 0 };
                writeln!(output, "P {a} {b} {c} {}", u32::from(poisoned)).unwrap();
            }
            _ => unreachable!(),
        }
    }
    assert!(slots.iter().all(Option::is_none));
}

fn script(epochs: &[Vec<Op>], include_info: bool) -> (Vec<u8>, Vec<u8>, usize) {
    let context = MemoryContext::new("assignment-oracle");
    let mut input = String::new();
    let mut output = String::new();
    if include_info {
        input.push_str("L\n");
        writeln!(
            output,
            "L {} {} 0 4 8 12 {FIRST_PART_OFFSET} {NUM_PARTS_INCLUDED}",
            mem::size_of::<ValueAssignment>(),
            mem::align_of::<ValueAssignment>()
        )
        .unwrap();
        let mut counts = vec![0, 1, 2, 3];
        for class in 1..=27 {
            let last = ((1u32 << (class + 4)) - 14) / 2;
            counts.extend([last - 1, last]);
            if class < 27 {
                counts.push(last + 1);
            }
        }
        counts.sort_unstable();
        counts.dedup();
        for count in counts {
            let info = AssignmentAllocationInfo::new(count).unwrap();
            writeln!(input, "I {count}").unwrap();
            writeln!(
                output,
                "I {count} {} {} {}",
                info.size, info.alloc_size, info.free_list_idx
            )
            .unwrap();
        }
    }
    with_assignment_allocator(context.mcx(), |mut allocator| {
        let mut ids = Ids::default();
        for ops in epochs {
            epoch(&allocator, ops, &mut ids, &mut input, &mut output);
            allocator.reset();
            ids.sync(allocator.stats());
            input.push_str("R\n");
            let stats = allocator.stats();
            writeln!(
                output,
                "R {} {}",
                stats.regular_slabs + stats.large_slabs,
                stats.allocated_bytes
            )
            .unwrap();
        }
    });
    assert_eq!(context.used(), 0);
    output.push_str("E 0 0\n");
    (
        input.into_bytes(),
        output.into_bytes(),
        epochs.iter().map(Vec::len).sum(),
    )
}

fn compile(executable: &Path, assertions: bool, flags: &[&str]) {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/value-assignment-reference");
    oracle::compile_cpp_with_flags(
        &base.join("tpde/include"),
        executable,
        &[
            base.join("oracle.cpp"),
            base.join("tpde/src/util/SmallVector.cpp"),
        ],
        assertions,
        flags,
    );
}

fn compare(executable: &Path, work: &Path, input: &[u8], expected: &[u8], label: &str) {
    let input_path = work.join("input.txt");
    fs::write(&input_path, input).unwrap();
    let result = Command::new(executable)
        .stdin(Stdio::from(fs::File::open(input_path).unwrap()))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{label}: C++ failed {}: {}",
        result.status,
        String::from_utf8_lossy(&result.stderr)
    );
    let actual = String::from_utf8(result.stdout).unwrap();
    let expected = std::str::from_utf8(expected).unwrap();
    let mut left = expected.lines();
    let mut right = actual.lines();
    let mut line = 1;
    loop {
        let a = left.next();
        let b = right.next();
        assert_eq!(
            a, b,
            "{label}: trace line {line} (operation and all fields shown)"
        );
        if a.is_none() {
            break;
        }
        line += 1;
    }
}

#[test]
fn full_assignment_fields_classes_and_allocator_match_pinned_cpp() {
    let work = std::env::temp_dir().join(format!("tpde-assignment-oracle-{}", std::process::id()));
    fs::create_dir_all(&work).unwrap();
    let (input, expected, operations) = script(&epochs(), true);
    let flags: &[&str] = if cfg!(tpde_address_sanitizer) {
        &["-fsanitize=address,undefined", "-fno-sanitize-recover=all"]
    } else {
        &[]
    };
    for assertions in [true, false] {
        let executable = work.join(format!("oracle-{assertions}"));
        compile(&executable, assertions, flags);
        compare(
            &executable,
            &work,
            &input,
            &expected,
            if assertions { "assertions" } else { "NDEBUG" },
        );
    }
    fs::remove_dir_all(work).unwrap();
    println!("TPDE_VALUE_ASSIGNMENT_ORACLE PASS epochs=5 cpp_modes=2 operations={} source_classes=28 active={}",
        operations * 2, usize::from(cfg!(tpde_address_sanitizer)));
}

#[test]
fn source_class_28_shift_is_diagnosed_and_rust_arithmetic_is_defined() {
    let first = ((1u32 << 31) - 14) / 2 + 1;
    let info = AssignmentAllocationInfo::new(first).unwrap();
    assert_eq!(info.free_list_idx, 28);
    assert_eq!(info.alloc_size, 1usize << 32);
    let work = std::env::temp_dir().join(format!("tpde-assignment-ub-{}", std::process::id()));
    fs::create_dir_all(&work).unwrap();
    let executable = work.join("oracle");
    compile(
        &executable,
        true,
        &["-fsanitize=address,undefined", "-fno-sanitize-recover=all"],
    );
    let (input, expected, _) = script(
        &[vec![
            Op('A', [0, 25, 2, 0xffffffff, 0]),
            Op('F', [0, 1, 0, 0, 0]),
        ]],
        true,
    );
    compare(
        &executable,
        &work,
        &input,
        &expected,
        "sanitized source-defined control",
    );
    let input_path = work.join("undefined.txt");
    fs::write(&input_path, format!("U {first}\n")).unwrap();
    let result = Command::new(&executable)
        .stdin(Stdio::from(fs::File::open(input_path).unwrap()))
        .output()
        .unwrap();
    assert!(!result.status.success());
    let diagnostic = String::from_utf8_lossy(&result.stderr);
    assert!(
        diagnostic.contains("shift exponent 32 is too large"),
        "unexpected source diagnostic: {diagnostic}"
    );
    fs::remove_dir_all(work).unwrap();
    println!("TPDE_VALUE_ASSIGNMENT_SOURCE_UB PASS class=28 rust_alloc_size=4294967296");
}

#[cfg(tpde_address_sanitizer)]
#[test]
fn active_asan_free_list_shadow_matches_source() {
    assert!(tpde_util::asan::ADDRESS_SANITIZER_ACTIVE);
    let mut operations = Vec::new();
    for count in [0, 1, 2, 8, 9, 24, 25, 8185, 32761] {
        let size = AssignmentAllocationInfo::new(count).unwrap().size;
        operations.extend([
            Op('A', [0, count, 0, 15, 0]),
            Op('P', [0, 0, 0, 0, 0]),
            Op('P', [0, size - 1, 0, 0, 0]),
            Op('F', [0, 0, 0, 0, 0]),
            Op('P', [0, 0, 1, 0, 0]),
            Op('P', [0, 8, 1, 0, 0]),
            Op('A', [0, count, 0, 16, 0]),
            Op('P', [0, 0, 0, 0, 0]),
            Op('P', [0, size - 1, 0, 0, 0]),
            Op('F', [0, 1, 0, 0, 0]),
        ]);
    }
    let work = std::env::temp_dir().join(format!("tpde-assignment-shadow-{}", std::process::id()));
    fs::create_dir_all(&work).unwrap();
    let executable = work.join("oracle");
    compile(
        &executable,
        true,
        &["-fsanitize=address,undefined", "-fno-sanitize-recover=all"],
    );
    let (input, expected, operations) = script(&[operations], false);
    compare(
        &executable,
        &work,
        &input,
        &expected,
        "active ASAN free-list shadow",
    );
    fs::remove_dir_all(work).unwrap();
    println!("TPDE_VALUE_ASSIGNMENT_ASAN PASS counts=9 active=1 operations={operations}");
}

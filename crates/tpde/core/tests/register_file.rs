// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![cfg(feature = "upstream-oracle")]

#[path = "../../test-support/oracle.rs"]
mod oracle;

use std::{fmt::Write, fs, mem, path::Path};
use tpde_base::ValLocalIdx;
use tpde_core::{Assignment, Reg, RegBank, RegisterFile};

type Op = (u8, u64, u64, u64);

fn out(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn snapshot<const B: usize, const R: usize>(
    file: &RegisterFile<B, R>,
    assigned: u64,
    result: u64,
    bytes: &mut Vec<u8>,
) {
    for value in [
        result,
        file.allocatable.get(),
        file.used.get(),
        file.clobbered.get(),
    ] {
        out(bytes, value);
    }
    for clock in &file.clocks {
        out(bytes, clock.get() as u64);
    }
    out(bytes, file.used.get().count_ones() as u64);
    for id in file.used_regs() {
        out(bytes, id);
    }
    for id in 0..RegisterFile::<B, R>::NUM_REGS {
        let reg = Reg::new(id as u64);
        for value in [
            file.is_used(reg),
            file.is_fixed(reg),
            file.is_clobbered(reg),
        ] {
            out(bytes, value as u64);
        }
        out(bytes, file.lock_counts[id / R][id % R].get() as u64);
        out(bytes, RegisterFile::<B, R>::reg_bank(reg).id() as u64);
        if assigned & (1u64 << id) != 0 {
            out(
                bytes,
                file.assignments[id / R][id % R].get().local_idx.get() as u64,
            );
            out(bytes, file.assignments[id / R][id % R].get().part as u64);
        }
        if file.is_used(reg) {
            out(bytes, file.reg_local_idx(reg).get() as u64);
            out(bytes, file.reg_part(reg) as u64);
        }
    }
    if R < 64 {
        for bank in [RegBank::new(0), RegBank::new(1)] {
            out(bytes, RegisterFile::<B, R>::bank_regs(bank));
            for exclusion in [
                0,
                u64::MAX,
                0x5555_5555_5555_5555,
                0xaaaa_aaaa_aaaa_aaaa,
                file.used.get(),
                !file.used.get(),
            ] {
                out(
                    bytes,
                    file.find_first_free_excluding(bank, exclusion).id() as u64,
                );
                out(
                    bytes,
                    file.find_first_nonfixed_excluding(bank, exclusion).id() as u64,
                );
            }
        }
    }
}

fn operations(total: usize, banks: usize) -> Vec<Op> {
    let mask = u64::MAX >> (64 - total);
    let mut ops = vec![(1, mask, 0, 0)];
    for bank in 0..banks {
        ops.push((11, bank as u64, 31 + bank as u64, 0));
    }
    for id in 0..total as u64 {
        ops.extend([
            (10, id, 0, 0),
            (2, id, id.wrapping_mul(1234567), u32::MAX as u64),
            (3, id, u32::MAX as u64 - id, id),
            (5, id, 0, 0),
            (7, id, 0, 0),
            (8, id, 0, 0),
            (6, id, 0, 0),
        ]);
    }
    for id in (0..total as u64).rev() {
        ops.push((5, id, 0, 0));
    }
    for id in (0..total as u64).rev() {
        ops.extend([(9, id, 1, 0), (4, id, 0, 0)]);
    }
    ops.extend([(2, 0, 0, 0), (9, 0, 0, 0)]);
    for _ in 0..255 {
        ops.push((7, 0, 0, 0));
    }
    for _ in 0..255 {
        ops.push((8, 0, 0, 0));
    }
    for _ in 0..255 {
        ops.push((7, 0, 0, 0));
    }
    ops.extend([(9, 0, 255, 0), (4, 0, 0, 0), (0, 0, 0, 0)]);
    for id in 0..total as u64 {
        ops.extend([(2, id, id + 100, id + 200), (7, id, 0, 0)]);
    }
    ops.push((0, 0, 0, 0));
    ops.push((1, 0xaaaa_aaaa_aaaa_aaaa & mask, 0, 0));
    for id in (0..total as u64).step_by(2) {
        ops.push((2, id, id + 400, id + 800));
    }
    ops.push((0, 0, 0, 0));
    ops
}

fn case<const B: usize, const R: usize>(input: &mut String, expected: &mut Vec<u8>) -> usize {
    let mut file = RegisterFile::<B, R>::new();
    let operations = operations(B * R, B);
    writeln!(input, "{B} {R} {}", operations.len()).unwrap();
    out(expected, mem::size_of::<RegisterFile<B, R>>() as u64);
    out(expected, mem::align_of::<RegisterFile<B, R>>() as u64);
    snapshot(&file, 0, 256, expected);
    let mut assigned = 0u64;
    for &(code, a, b, c) in &operations {
        writeln!(input, "{code} {a} {b} {c}").unwrap();
        let mut result = 256;
        match code {
            0 => file.reset(),
            1 => file.allocatable.set(a),
            2 => {
                file.mark_used(Reg::new(a), ValLocalIdx::new(b as u32), c as u32);
                assigned |= 1u64 << a;
            }
            3 => file.update_reg_assignment(Reg::new(a), ValLocalIdx::new(b as u32), c as u32),
            4 => file.unmark_used(Reg::new(a)),
            5 => file.mark_fixed(Reg::new(a)),
            6 => file.unmark_fixed(Reg::new(a)),
            7 => file.inc_lock_count(Reg::new(a)),
            8 => result = file.dec_lock_count(Reg::new(a)) as u64,
            9 => file.dec_lock_count_must_zero(Reg::new(a), b as u8),
            10 => file.mark_clobbered(Reg::new(a)),
            11 => file.clocks[a as usize].set(b as u8),
            _ => unreachable!(),
        }
        snapshot(&file, assigned, result, expected);
    }
    operations.len() + 1
}

#[test]
fn pinned_register_file_operations_match_both_cpp_modes() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR"));
    let work = std::env::temp_dir().join(format!("tpde-register-file-{}", std::process::id()));
    fs::create_dir_all(&work).unwrap();
    let mut expected = Vec::new();
    for value in [
        mem::size_of::<Reg>(),
        mem::align_of::<Reg>(),
        mem::size_of::<RegBank>(),
        mem::align_of::<RegBank>(),
        mem::size_of::<Assignment>(),
        mem::align_of::<Assignment>(),
    ] {
        out(&mut expected, value as u64);
    }
    out(&mut expected, RegBank::default().id() as u64);
    for id in 0..=255 {
        let reg = Reg::new(id);
        for value in [
            reg.id() as u64,
            reg.invalid() as u64,
            reg.valid() as u64,
            (reg == Reg::make_invalid()) as u64,
            RegBank::new(id as u8).id() as u64,
            (RegBank::new(id as u8) == RegBank::default()) as u64,
            RegisterFile::<2, 32>::reg_bank(reg).id() as u64,
        ] {
            out(&mut expected, value);
        }
    }
    let mut input = String::new();
    let snapshots = case::<1, 1>(&mut input, &mut expected)
        + case::<2, 1>(&mut input, &mut expected)
        + case::<1, 32>(&mut input, &mut expected)
        + case::<2, 32>(&mut input, &mut expected)
        + case::<4, 16>(&mut input, &mut expected)
        + case::<1, 64>(&mut input, &mut expected);
    for assertions in [true, false] {
        let executable = work.join(format!("oracle-{assertions}"));
        oracle::compile_cpp_with_flags(
            &base.join("tests/register-reference/tpde/include"),
            &executable,
            &[base.join("tests/register_file_oracle.cpp")],
            assertions,
            &[],
        );
        oracle::compare_bytes(
            &executable,
            &work,
            input.as_bytes(),
            &expected,
            "RegisterFile operations",
        );
    }
    fs::remove_dir_all(work).unwrap();
    println!("TPDE_REGISTER_FILE_ORACLE PASS shapes=6 register_forms=256 snapshots={snapshots} cpp_modes=2");
}

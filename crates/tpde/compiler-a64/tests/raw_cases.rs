// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#[derive(Clone, Debug)]
pub struct Operation {
    pub code: u64,
    pub args: [u64; 8],
}
pub fn op(code: u64, args: &[u64]) -> Operation {
    let mut operation = Operation { code, args: [0; 8] };
    operation.args[..args.len()].copy_from_slice(args);
    operation
}
pub struct Case {
    pub name: &'static str,
    pub mode: u64,
    pub operations: Vec<Operation>,
}
pub fn cases() -> Vec<Case> {
    let mut cases = Vec::new();
    let mut operations = Vec::new();
    for &(dst, src) in &[(0, 1), (30, 0), (32, 63), (63, 32), (2, 33), (62, 3)] {
        for size in [1, 2, 4, 8] {
            operations.push(op(3, &[dst, src, size]));
        }
    }
    operations.push(op(3, &[32, 63, 16]));
    operations.push(op(3, &[0, 0, 8]));
    operations.push(op(3, &[32, 32, 16]));
    cases.push(Case {
        name: "move-banks-widths-aliases",
        mode: 0,
        operations,
    });
    let mut operations = Vec::new();
    for dst in [0, 30, 32, 63] {
        for size in [1, 2, 4, 8, 16] {
            if dst < 32 && size > 8 {
                continue;
            }
            for off in [0, 0xfff * size, 0x1000 * size, 0x1234_5670] {
                for base in [9, 31] {
                    if base == 31 && off == 0x1234_5670 {
                        continue;
                    }
                    operations.push(op(1, &[dst, base, off, size, 0]));
                    operations.push(op(2, &[base, off, dst, size]));
                    if dst < 32 {
                        operations.push(op(1, &[dst, base, off, size, 1]));
                    }
                }
            }
        }
    }
    cases.push(Case {
        name: "memory-scalar-vector-sign-offsets",
        mode: 0,
        operations,
    });
    let mut operations = Vec::new();
    for value in [
        0,
        1,
        0xffff,
        u64::MAX,
        0xffff_ffff,
        0x1234_0000,
        0xffff_edcb_ffff_ffff,
        0xff00_ff00_ff00_ff00,
        0x1234_5678,
        0x1234_5678_9abc,
        0x1234_5678_9abc_def0,
    ] {
        for size in [1, 2, 4, 8] {
            operations.push(op(4, &[0, size, 5, value, 0]));
        }
    }
    for (size, low, high) in [
        (1, 0x19, 0),
        (2, 0x9999, 0),
        (4, 1.0f32.to_bits() as u64, 0),
        (4, (-1.0f32).to_bits() as u64, 0),
        (4, 0, 0),
        (4, 0x3f80_0001, 0),
        (4, 0x7fc0_0001, 0),
        (4, 0xffff_ffff, 0),
        (8, 1.0f64.to_bits(), 0),
        (8, (-1.0f64).to_bits(), 0),
        (8, 0, 0),
        (8, 0x3ff0_0000_0000_0001, 0),
        (8, 0x7ff8_0000_0000_0001, 0),
        (8, 0xff00_ff00_ff00_ff00, 0),
        (16, 0, 0),
        (16, u64::MAX, u64::MAX),
        (16, 0xff00_ff00_ff00_ff00, 0),
        (16, 0x1234_5678_9abc_def0, 0xfedc_ba98_7654_3210),
    ] {
        operations.push(op(4, &[1, size, 39, low, high]));
    }
    cases.push(Case {
        name: "constant-gp-fp-immediate-and-rodata",
        mode: 0,
        operations,
    });
    let mut operations = Vec::new();
    for condition in 0..14 {
        operations.push(op(5, &[condition, 0]));
        operations.push(op(6, &[condition, 1]));
        for width in [0, 1] {
            operations.push(op(7, &[condition, 2, 3, 4, width]));
        }
        operations.push(op(19, &[condition, 0]));
        if ![4, 5, 6, 7].contains(&condition) {
            operations.push(op(19, &[condition, 1]));
        }
    }
    operations.push(op(19, &[14, 0]));
    operations.push(op(19, &[14, 1]));
    for kind in 15..19 {
        for register in [0, 30] {
            for width_or_bit in [0, 1, 31, 63] {
                operations.push(op(20, &[kind, register, width_or_bit]));
            }
        }
    }
    for from in [1, 7, 8, 15, 16, 31, 32, 63] {
        for to in [8, 16, 32, 64] {
            if from >= to {
                continue;
            }
            for sign in [0, 1] {
                operations.push(op(8, &[2, 3, sign, from, to]));
            }
        }
    }
    for &(lsb, width) in &[
        (0, 1),
        (0, 64),
        (1, 63),
        (7, 17),
        (31, 32),
        (32, 32),
        (63, 1),
    ] {
        operations.push(op(9, &[4, 5, lsb, width]));
        operations.push(op(10, &[4, 5, lsb, width]));
    }
    for features in [0, 1, 0x8000_0000, u32::MAX as u64] {
        for requested in [0, 1, 0x8000_0000, u32::MAX as u64] {
            operations.push(op(18, &[features, requested]));
        }
    }
    cases.push(Case {
        name: "conditions-extensions-bitfields-features",
        mode: 0,
        operations,
    });
    let mut operations = vec![op(12, &[]), op(12, &[])];
    for kind in 0..19 {
        operations.push(op(11, &[kind, 3, kind % 2, kind * 3 % 64, 0]));
    }
    operations.push(op(14, &[96]));
    operations.push(op(13, &[0]));
    for kind in 0..19 {
        operations.push(op(11, &[kind, 30, kind % 2, 63, 0]));
    }
    operations.push(op(13, &[1]));
    operations.push(op(11, &[14, 0, 0, 0, 1]));
    cases.push(Case {
        name: "all-forward-and-backward-jump-kinds",
        mode: 0,
        operations,
    });
    for (name, kind, distance, width, bit) in [
        ("backward-tbz-bit31-exact", 17, 32768, 0, 31),
        ("backward-tbz-bit31-expanded", 17, 32772, 0, 31),
        ("backward-tbz-bit63-exact", 17, 32768, 0, 63),
        ("backward-tbz-bit63-expanded", 17, 32772, 0, 63),
        ("backward-tbnz-bit31-exact", 18, 32768, 0, 31),
        ("backward-tbnz-bit31-expanded", 18, 32772, 0, 31),
        ("backward-tbnz-bit63-exact", 18, 32768, 0, 63),
        ("backward-tbnz-bit63-expanded", 18, 32772, 0, 63),
        ("backward-cbz-x-exact", 15, 1048576, 0, 0),
        ("backward-cbz-x-expanded", 15, 1048580, 0, 0),
        ("backward-cbz-w-exact", 15, 1048576, 1, 0),
        ("backward-cbz-w-expanded", 15, 1048580, 1, 0),
        ("backward-cbnz-x-exact", 16, 1048576, 0, 0),
        ("backward-cbnz-x-expanded", 16, 1048580, 0, 0),
        ("backward-cbnz-w-exact", 16, 1048576, 1, 0),
        ("backward-cbnz-w-expanded", 16, 1048580, 1, 0),
        ("backward-eq-exact", 0, 1048576, 0, 0),
        ("backward-eq-expanded", 0, 1048580, 0, 0),
        ("backward-le-exact", 13, 1048576, 0, 0),
        ("backward-le-expanded", 13, 1048580, 0, 0),
    ] {
        let operations = vec![
            op(12, &[]),
            op(13, &[0]),
            op(14, &[distance]),
            op(11, &[kind, 3, width, bit, 0]),
        ];
        cases.push(Case {
            name,
            mode: 0,
            operations,
        });
    }
    let mut operations = vec![op(12, &[])];
    for index in 0..8 {
        operations.push(op(11, &[17 + index % 2, 3, 0, index, 0]));
    }
    operations.push(op(14, &[65536]));
    operations.push(op(13, &[0]));
    cases.push(Case {
        name: "pending-test-branch-veneer-growth",
        mode: 0,
        operations,
    });
    let mut operations = vec![op(12, &[]), op(12, &[]), op(12, &[])];
    for width in [0, 1] {
        for value in [0, 1, 4095, 4096, 0xfff000, 0x1234_5678, u64::MAX] {
            operations.push(op(15, &[0, 3, 4, value, width]));
            operations.push(op(16, &[0, 1, 3, 4, value, width]));
        }
        for (low, high) in [(0, 0), (1, 5), (0x1234_5678, 0x1234_567c)] {
            operations.push(op(17, &[0, 3, 4, low, high, width, 1, 2]));
        }
    }
    for label in 0..3 {
        operations.push(op(13, &[label]));
        operations.push(op(14, &[4]));
    }
    cases.push(Case {
        name: "switch-immediates-binary-steps-and-tables",
        mode: 0,
        operations,
    });
    let mut operations = vec![op(3, &[])];
    for bank in [0, 1] {
        operations.push(op(0, &[]));
        for index in 0..12 {
            operations.push(op(
                1,
                &[
                    bank,
                    if bank == 0 { 8 } else { 16 },
                    if index == 2 { 16 } else { 8 },
                    index % 3,
                    0,
                    0,
                    255,
                    0,
                ],
            ));
        }
        operations.push(op(0, &[]));
        for index in 0..6 {
            operations.push(op(
                2,
                &[bank, 8, if index == 2 { 16 } else { 8 }, 0, 0, 0, 255, 0],
            ));
        }
    }
    operations.push(op(0, &[]));
    for (size, align) in [(1, 1), (9, 8), (17, 16), (32, 32)] {
        operations.push(op(1, &[0, size, align, 0, 2, 0, 255, 0]));
    }
    operations.push(op(1, &[0, 8, 8, 0, 1, 0, 255, 0]));
    for bank in [0, 1] {
        for _ in 0..10 {
            operations.push(op(1, &[bank, 8, 8, 0, 0, 0, 255, 0]));
        }
    }
    cases.push(Case {
        name: "aapcs-reset-banks-align-byval-sret-returns",
        mode: 1,
        operations,
    });
    cases
}

pub fn encode(case: &Case) -> Vec<u8> {
    let mut input = Vec::new();
    input.extend_from_slice(&case.mode.to_le_bytes());
    input.extend_from_slice(&(case.operations.len() as u64).to_le_bytes());
    for operation in &case.operations {
        input.extend_from_slice(&operation.code.to_le_bytes());
        for value in operation.args {
            input.extend_from_slice(&value.to_le_bytes());
        }
    }
    input
}

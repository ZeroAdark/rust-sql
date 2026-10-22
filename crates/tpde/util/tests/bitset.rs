// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![cfg(feature = "upstream-oracle")]

#[path = "../../test-support/oracle.rs"]
mod oracle;
use mcx::{Mcx, MemoryContext};
use std::{fs, path::Path, process::Command};
use tpde_util::SmallBitSet;

fn out(dst: &mut Vec<u8>, value: u64) {
    dst.extend_from_slice(&value.to_le_bytes());
}

struct Script<'m, const WORDS: usize> {
    sets: [SmallBitSet<'m, WORDS>; 2],
    input: Vec<u8>,
    expected: Vec<u8>,
    operations: u32,
}
impl<'m, const WORDS: usize> Script<'m, WORDS> {
    fn new(mcx: Mcx<'m>) -> Self {
        let mut result = Self {
            sets: core::array::from_fn(|_| SmallBitSet::new(mcx)),
            input: vec![WORDS as u8, 0, 0, 0, 0],
            expected: Vec::new(),
            operations: 0,
        };
        result.snapshot();
        result
    }
    fn snapshot(&mut self) {
        for set in &self.sets {
            out(&mut self.expected, set.bit_size() as u64);
            out(&mut self.expected, set.words().len() as u64);
            out(
                &mut self.expected,
                set.first_set().map_or(u64::MAX, u64::from),
            );
            for (idx, &word) in set.words().iter().enumerate() {
                out(&mut self.expected, word);
                let mut observed = 0;
                for bit in 0..64 {
                    observed |= u64::from(set.is_set(idx as u32 * 64 + bit).unwrap()) << bit;
                }
                out(&mut self.expected, observed);
            }
        }
    }
    fn op(&mut self, op: u8, a: usize, b: usize, n: u32, value: u64) {
        self.input.extend_from_slice(&[op, a as u8, b as u8, 0]);
        self.input.extend_from_slice(&n.to_le_bytes());
        out(&mut self.input, value);
        match op {
            0 | 9 => {}
            1 => self.sets[a].clear(),
            2 => self.sets[a].resize(n).unwrap(),
            3 => self.sets[a].zero(),
            4 => self.sets[a].push_back(n != 0).unwrap(),
            5 => self.sets[a].mark_set(n).unwrap(),
            6 => self.sets[a].mark_unset(n).unwrap(),
            7 => {
                let moved = SmallBitSet::move_from(&mut self.sets[b]);
                self.sets[a] = moved;
            }
            8 => {
                let (left, right) = self.sets.split_at_mut(1);
                if a == 0 {
                    left[0].move_assign(&right[0]);
                } else {
                    right[0].move_assign(&left[0]);
                }
            }
            10 => self.sets[a].words_mut()[n as usize] = value,
            _ => unreachable!(),
        }
        self.operations += 1;
        self.snapshot();
    }
    fn simple(&mut self, op: u8, n: u32) {
        self.op(op, 0, 0, n, 0);
    }
    fn finish(mut self) -> (Vec<u8>, Vec<u8>, u32) {
        self.input[1..5].copy_from_slice(&self.operations.to_le_bytes());
        (self.input, self.expected, self.operations)
    }
}

fn script<const WORDS: usize>() -> (Vec<u8>, Vec<u8>, u32) {
    let context = MemoryContext::new_bump("bitset-oracle");
    let mut s = Script::<WORDS>::new(context.mcx());
    for size in [
        0, 1, 63, 64, 65, 127, 128, 129, 255, 256, 257, 511, 512, 513,
    ] {
        s.simple(1, 0);
        s.simple(2, size);
        for word in 0..s.sets[0].words().len() {
            s.op(10, 0, 0, word as u32, u64::MAX);
        }
        if size != 0 {
            s.simple(6, size - 1);
            s.simple(5, size - 1);
            s.simple(2, size / 2);
        }
        s.simple(2, size + 1);
        s.simple(3, 0);
    }
    s.simple(1, 0);
    for i in 0..600 {
        s.simple(4, (i % 3 == 0) as u32);
    }
    for bit in [0, 63, 64, 127, 128, 255, 256, 511, 599, 639] {
        s.simple(3, 0);
        s.simple(5, bit);
        s.simple(6, bit);
    }
    let mut random = 0x09b16d273c1e8905u64;
    for _ in 0..512 {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        match random % 6 {
            0 => s.simple(2, ((random >> 8) % 700) as u32),
            1 => s.simple(4, ((random >> 8) & 1) as u32),
            2 => s.simple(3, 0),
            3 | 4 if !s.sets[0].words().is_empty() => {
                let bit = ((random >> 8) % (s.sets[0].words().len() as u64 * 64)) as u32;
                s.simple(if random % 6 == 3 { 5 } else { 6 }, bit);
            }
            _ => s.simple(0, 0),
        }
    }
    for source in [0, 1, 63, 64, 65, 128, 256, 257] {
        for dest in [0, 1, 65, 257] {
            for move_op in [7, 8] {
                for (a, size) in [(0, source), (1, dest)] {
                    s.op(1, a, 0, 0, 0);
                    s.op(2, a, 0, size, 0);
                    if size != 0 {
                        s.op(5, a, 0, size - 1, 0);
                    }
                }
                s.op(move_op, 1, 0, 0, 0);
                s.op(9, 1, 0, 0, 0);
                s.op(3, 0, 0, 0, 0);
                s.op(2, 0, 0, source, 0);
                s.op(2, 1, 0, source, 0);
            }
        }
    }
    s.simple(1, 0);
    s.simple(2, 64);
    s.op(7, 1, 0, 0, 0);
    s.simple(4, 0);
    s.simple(2, 66);
    for size in u32::MAX - 62..=u32::MAX {
        s.simple(1, 0);
        s.simple(2, 64);
        s.simple(5, 63);
        s.simple(2, size);
        s.simple(3, 0);
        s.simple(2, 65);
    }
    s.finish()
}

fn rejected_cpp_forms(include: &Path, work: &Path, assertions: bool) {
    let cxx = std::env::var("CXX").unwrap_or_else(|_| "clang++".into());
    for (name, operation, diagnostic) in [
        ("copy-ctor", "auto copy = original;", "deleted"),
        (
            "copy-assign",
            "tpde::util::SmallBitSet<64> copy; copy = original;",
            "deleted",
        ),
        (
            "bad-inline",
            "tpde::util::SmallBitSet<65> invalid;",
            "static assertion",
        ),
    ] {
        let source = work.join(format!("{name}.cpp"));
        fs::write(&source, format!("#include <cstdlib>\n#include <cassert>\n#include \"tpde/util/SmallBitSet.hpp\"\nvoid f() {{ tpde::util::SmallBitSet<64> original; {operation} }}\n")).unwrap();
        let mut command = Command::new(&cxx);
        command
            .args(["-std=c++20", "-fsyntax-only", "-I"])
            .arg(include)
            .arg(&source);
        if !assertions {
            command.arg("-DNDEBUG");
        }
        let result = command.output().unwrap();
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(
            !result.status.success()
                && stderr.contains(diagnostic)
                && (name == "bad-inline" || stderr.contains("SmallVector")),
            "{name}: wrong compilation outcome: {}\n{stderr}",
            result.status
        );
    }
}

#[test]
fn pinned_cpp_bitset_differential() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let include = root.join("tests/upstream/tpde/include");
    let work = std::env::temp_dir().join(format!("tpde-bitset-{}", std::process::id()));
    fs::create_dir_all(&work).unwrap();
    let cases = [script::<0>(), script::<1>(), script::<4>()];
    let operations: u32 = cases.iter().map(|(_, _, count)| count).sum();
    assert!(operations > 6000);
    for assertions in [true, false] {
        let executable = work.join(if assertions {
            "oracle-debug"
        } else {
            "oracle-release"
        });
        oracle::compile_cpp(
            &include,
            &executable,
            &[
                root.join("tests/bitset_oracle.cpp"),
                root.join("tests/upstream/tpde/src/util/SmallVector.cpp"),
            ],
            assertions,
        );
        rejected_cpp_forms(&include, &work, assertions);
        for (idx, (input, expected, _)) in cases.iter().enumerate() {
            oracle::compare_bytes(
                &executable,
                &work,
                input,
                expected,
                &format!("bitset-{idx}-assertions-{assertions}"),
            );
        }
    }
    fs::remove_dir_all(work).unwrap();
    println!(
        "TPDE_BITSET_ORACLE PASS capacities=3 cpp_modes=2 operations={} negative_forms=6",
        operations * 2
    );
}

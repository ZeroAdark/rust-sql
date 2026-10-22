// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![cfg(feature = "upstream-oracle")]

#[path = "../../test-support/oracle.rs"]
mod oracle;

use mcx::MemoryContext;
use std::{cell::Cell, fmt::Write, fs, path::Path};
use tpde_util::{BumpAllocator, BumpStats};

thread_local! { static DESTROYED: Cell<usize> = const { Cell::new(0) }; }
struct Tracked(u64);
impl Drop for Tracked {
    fn drop(&mut self) {
        DESTROYED.with(|count| count.set(count.get() + 1));
    }
}

#[derive(Clone, Copy)]
enum Op {
    Allocate(usize, usize),
    Slab(usize, usize),
    Reset,
    Destructors,
}

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

fn commands(slab: usize) -> Vec<Op> {
    let mut result = vec![Op::Reset, Op::Allocate(slab + 1, 16), Op::Reset];
    for index in 0..41 {
        result.push(Op::Slab(1 + index % 5, 1));
        result.push(Op::Allocate(7 + index % 23, 1 << (index % 5)));
    }
    result.push(Op::Destructors);
    result.push(Op::Reset);
    let mut random = 0x1a2b_3c4d_5e6f_7081u64;
    for index in 0..600 {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        result.push(Op::Allocate(
            1 + random as usize % (slab * 3),
            1 << (index % 5),
        ));
        if index % 37 == 0 {
            result.push(Op::Destructors);
        }
        if index % 91 == 0 {
            result.push(Op::Reset);
        }
    }
    result.extend([Op::Reset, Op::Destructors, Op::Allocate(1, 1), Op::Reset]);
    result
}

fn script<const S: usize>() -> (Vec<u8>, Vec<u8>, usize) {
    let context = MemoryContext::new("bump-oracle");
    let mut bump = BumpAllocator::<S>::new(context.mcx());
    let mut ids = Ids::default();
    let mut input = format!("{S}\n");
    let mut output = String::new();
    let commands = commands(S);
    for command in &commands {
        match *command {
            Op::Allocate(size, align) | Op::Slab(size, align) => {
                let force = matches!(command, Op::Slab(..));
                let op = if force { 'S' } else { 'A' };
                writeln!(input, "{op} {size} {align}").unwrap();
                let bytes = if force {
                    bump.allocate_slab(size, align)
                } else {
                    bump.allocate(size, align)
                }
                .unwrap();
                for byte in bytes.iter_mut() {
                    byte.write(0x5a);
                }
                let info = bump.allocation_info(bytes.as_ptr().cast()).unwrap();
                let stats = bump.stats();
                ids.sync(stats);
                let id = if info.large {
                    ids.large[info.index]
                } else {
                    ids.regular[info.index]
                };
                writeln!(
                    output,
                    "{op} {id} {} {} {} {}",
                    info.offset,
                    info.size,
                    stats.regular_slabs + stats.large_slabs,
                    stats.allocated_bytes
                )
                .unwrap();
            }
            Op::Reset => {
                input.push_str("R\n");
                bump.reset();
                let stats = bump.stats();
                ids.sync(stats);
                writeln!(
                    output,
                    "R {} {}",
                    stats.regular_slabs + stats.large_slabs,
                    stats.allocated_bytes
                )
                .unwrap();
            }
            Op::Destructors => {
                input.push_str("D\n");
                DESTROYED.with(|count| count.set(0));
                let first = bump.new_with(|| Ok(Tracked(41))).unwrap();
                ids.sync(bump.stats());
                let moved = first;
                assert_eq!(moved.0, 41);
                drop(moved);
                let second = bump.new_with(|| Ok(Tracked(43))).unwrap();
                ids.sync(bump.stats());
                let released = second.into_inner();
                let value = released.0;
                drop(released);
                let array = bump
                    .new_array_with(3, |index| Ok([2u64, 3, 5][index]))
                    .unwrap();
                ids.sync(bump.stats());
                let sum: u64 = array.iter().sum();
                drop(array);
                let failure = || Err(types_error::PgError::error("constructor failed").into());
                assert!(bump.new_with::<u64>(failure).is_err());
                ids.sync(bump.stats());
                assert!(bump.new_array_with::<u64>(2, |_| failure()).is_err());
                ids.sync(bump.stats());
                assert_eq!(DESTROYED.with(Cell::get), 2);
                let stats = bump.stats();
                writeln!(
                    output,
                    "D 1 {value} {sum} 2 2 {} {}",
                    stats.regular_slabs + stats.large_slabs,
                    stats.allocated_bytes
                )
                .unwrap();
            }
        }
    }
    drop(bump);
    assert_eq!(context.used(), 0);
    output.push_str("E 0 0\n");
    (input.into_bytes(), output.into_bytes(), commands.len())
}

#[test]
fn complete_bump_allocator_matches_pinned_cpp() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let work = std::env::temp_dir().join(format!("tpde-bump-oracle-{}", std::process::id()));
    fs::create_dir_all(&work).unwrap();
    let cases = [script::<64>(), script::<128>(), script::<4096>()];
    let flags: &[&str] = if cfg!(tpde_address_sanitizer) {
        &["-fsanitize=address", "-DTPDE_ASAN_BUILD"]
    } else {
        &[]
    };
    for assertions in [true, false] {
        let executable = work.join(if assertions { "assertions" } else { "ndebug" });
        oracle::compile_cpp_with_flags(
            &root.join("tests/bump-reference/tpde/include"),
            &executable,
            &[
                root.join("tests/bump_allocator_oracle.cpp"),
                root.join("tests/bump-reference/tpde/src/util/SmallVector.cpp"),
            ],
            assertions,
            flags,
        );
        for (index, (input, expected, _)) in cases.iter().enumerate() {
            oracle::compare_bytes(
                &executable,
                &work,
                input,
                expected,
                &format!("bump-script-{index}"),
            );
        }
        #[cfg(tpde_address_sanitizer)]
        compare_shadow(&executable, &work);
    }
    println!(
        "TPDE_BUMP_ORACLE PASS scripts=3 cpp_modes=2 operations={} active={}",
        cases.iter().map(|case| case.2).sum::<usize>() * 2,
        usize::from(cfg!(tpde_address_sanitizer))
    );
    fs::remove_dir_all(work).unwrap();
}

#[cfg(tpde_address_sanitizer)]
fn compare_shadow(executable: &Path, work: &Path) {
    extern "C" {
        fn __asan_address_is_poisoned(pointer: *const u8) -> i32;
    }
    let context = MemoryContext::new("bump-shadow");
    let mut bump = BumpAllocator::<64>::new(context.mcx());
    let first = bump.allocate(8, 8).unwrap().as_ptr().cast::<u8>();
    let mut output = String::from("A 0 0 64 1 64\n");
    let mut query = |offset| {
        // SAFETY: query reads only ASAN shadow, within the retained first slab allocation.
        let poisoned = unsafe { __asan_address_is_poisoned(first.add(offset)) != 0 };
        writeln!(output, "P {}", usize::from(poisoned)).unwrap();
    };
    query(0);
    query(8);
    bump.reset();
    output.push_str("R 1 64\n");
    // SAFETY: retained slab is live, though poisoned after reset.
    writeln!(output, "P {}", unsafe {
        usize::from(__asan_address_is_poisoned(first) != 0)
    })
    .unwrap();
    bump.allocate(16, 16).unwrap();
    output.push_str("A 0 0 64 1 64\n");
    for offset in [15, 16] {
        // SAFETY: only the shadow of the retained live slab is observed.
        writeln!(output, "P {}", unsafe {
            usize::from(__asan_address_is_poisoned(first.add(offset)) != 0)
        })
        .unwrap();
    }
    drop(bump);
    output.push_str("E 0 0\n");
    oracle::compare_bytes(
        executable,
        work,
        b"64\nA 8 8\nP 0 0\nP 0 8\nR\nP 0 0\nA 16 16\nP 0 15\nP 0 16\n",
        output.as_bytes(),
        "bump-asan-shadow",
    );
}

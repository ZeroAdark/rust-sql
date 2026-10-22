// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![cfg(feature = "upstream-oracle")]

#[path = "../../test-support/oracle.rs"]
mod oracle;

use mcx::MemoryContext;
use std::{cell::RefCell, fs, path::Path, process::Command, rc::Rc};
use tpde_util::SegmentedVector;

type Events = Rc<RefCell<Vec<[u64; 3]>>>;

#[repr(align(128))]
struct Tracked {
    id: u64,
    value: u64,
    events: Events,
}

impl Tracked {
    fn new(id: u64, value: u64, events: &Events) -> Self {
        events.borrow_mut().push([1, id, value]);
        Self {
            id,
            value,
            events: events.clone(),
        }
    }
}

impl Clone for Tracked {
    fn clone(&self) -> Self {
        self.events.borrow_mut().push([2, self.id, self.value]);
        Self {
            id: self.id,
            value: self.value,
            events: self.events.clone(),
        }
    }
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.events.borrow_mut().push([3, self.id, self.value]);
    }
}

fn write(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn events_snapshot(out: &mut Vec<u8>, events: &Events) {
    let events = events.borrow();
    write(out, events.len() as u64);
    for event in events.iter() {
        for value in event {
            write(out, *value);
        }
    }
}

fn snapshot<const N: usize>(
    set: &mut SegmentedVector<'_, Tracked, N>,
    addresses: &mut Vec<usize>,
    out: &mut Vec<u8>,
    events: &Events,
) {
    assert_eq!(set.len(), set.size());
    assert_eq!(set.is_empty(), set.len() == 0);
    assert!(set.get(set.len()).is_none());
    let end = set.len();
    assert!(set.get_mut(end).is_none());
    write(out, set.len() as u64);
    for i in 0..set.len() {
        let element = &set[i];
        let pointer = element as *const Tracked as usize;
        write(out, element.id);
        write(out, element.value);
        write(out, (pointer % std::mem::align_of::<Tracked>() == 0) as u64);
        let contiguous = i % N == 0 || pointer == addresses[i - 1] + std::mem::size_of::<Tracked>();
        write(out, contiguous as u64);
        if i == addresses.len() {
            addresses.push(pointer);
        }
        write(out, (pointer == addresses[i]) as u64);
        write(out, std::ptr::eq(&set[i], set.get(i).unwrap()) as u64);
        let mutable = set.get_mut(i).unwrap() as *mut Tracked as usize;
        write(out, (pointer == mutable) as u64);
    }
    events_snapshot(out, events);
}

fn script<const N: usize>() -> (Vec<u8>, Vec<u8>, usize) {
    let context = MemoryContext::new_bump("segmented-oracle");
    let events: Events = Rc::default();
    let mut set = SegmentedVector::<Tracked, N>::new(context.mcx());
    let mut input = vec![N as u8];
    let mut expected = Vec::new();
    let mut addresses = Vec::new();
    let mut next_id = 1;
    let ops: &[(u8, usize, u64)] = &[
        (0, 0, 0),
        (1, 0, 0),
        (1, 0, 0),
        (2, 1, 10),
        (6, N * 2 + 1, 20),
        (4, N + 1, 40),
        (5, 0, 90),
        (5, N, 91),
        (3, N * 34 + 1, 50),
        (5, N * 33, 92),
        (1, 0, 0),
        (1, 0, 0),
        (3, N - 1, 60),
        (2, 1, 70),
        (2, 1, 71),
        (1, 0, 0),
        (4, N * 37 + 3, 80),
        (1, 0, 0),
        (2, N * 2, 100),
        (1, 0, 0),
        (3, N * 39 + 1, 110),
    ];
    input.extend_from_slice(&(ops.len() as u32).to_le_bytes());
    snapshot(&mut set, &mut addresses, &mut expected, &events);
    for &(op, n, value) in ops {
        input.push(op);
        input.extend_from_slice(&(n as u32).to_le_bytes());
        write(&mut input, value);
        match op {
            0 => {}
            1 => set.clear(),
            2 | 3 => {
                for j in 0..n {
                    if op == 2 {
                        set.push_back(Tracked::new(next_id, value + j as u64, &events))
                            .unwrap();
                    } else {
                        let inserted = set
                            .emplace_back(|| Tracked::new(next_id, value + j as u64, &events))
                            .unwrap();
                        assert_eq!(inserted.id, next_id);
                    }
                    next_id += 1;
                }
            }
            4 => {
                let source = Tracked::new(next_id, value, &events);
                next_id += 1;
                for _ in 0..n {
                    set.push_copy(&source).unwrap();
                }
            }
            5 => set[n].value = value,
            6 => {
                let first = &set[0];
                let first_id = first.id;
                for j in 0..n {
                    set.push_back(Tracked::new(next_id, value + j as u64, &events))
                        .unwrap();
                    next_id += 1;
                }
                assert_eq!(first.id, first_id);
                assert!(std::ptr::eq(first, &set[0]));
            }
            _ => unreachable!(),
        }
        snapshot(&mut set, &mut addresses, &mut expected, &events);
    }
    drop(set);
    events_snapshot(&mut expected, &events);
    (input, expected, ops.len())
}

fn rejected_cpp_forms(include: &Path, work: &Path, assertions: bool) {
    let cxx = std::env::var("CXX").unwrap_or_else(|_| "clang++".into());
    for (name, operation) in [
        ("copy-construction", "auto second = original;"),
        (
            "copy-assignment",
            "tpde::util::SegmentedVector<int> second; second = original;",
        ),
        ("move-construction", "auto second = std::move(original);"),
        (
            "move-assignment",
            "tpde::util::SegmentedVector<int> second; second = std::move(original);",
        ),
    ] {
        let source = work.join(format!("{name}.cpp"));
        fs::write(&source, format!("#include <cstdlib>\n#include <utility>\n#include \"tpde/util/SegmentedVector.hpp\"\nvoid f() {{ tpde::util::SegmentedVector<int> original; {operation} }}\n")).unwrap();
        let mut command = Command::new(&cxx);
        command
            .args(["-std=c++20", "-fsyntax-only", "-I"])
            .arg(include)
            .arg(&source);
        if !assertions {
            command.arg("-DNDEBUG");
        }
        let result = command.output().unwrap();
        let diagnostics = String::from_utf8_lossy(&result.stderr);
        assert!(
            !result.status.success()
                && diagnostics.contains("deleted")
                && diagnostics.contains("SegmentedVector"),
            "{name}: {}\n{diagnostics}",
            result.status
        );
    }
}

#[test]
fn pinned_cpp_segmented_vector_differential() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let include = root.join("tests/segmented-reference/tpde/include");
    let work = std::env::temp_dir().join(format!("tpde-segmented-{}", std::process::id()));
    fs::create_dir_all(&work).unwrap();
    let cases = [script::<1>(), script::<3>(), script::<64>()];
    let operations: usize = cases.iter().map(|(_, _, operations)| operations).sum();
    for (mode, assertions, flags) in [
        ("assertions", true, &[][..]),
        ("ndebug", false, &[][..]),
        (
            "sanitizers",
            true,
            &[
                "-fsanitize=address,undefined",
                "-fno-sanitize-recover=all",
                "-fno-omit-frame-pointer",
            ][..],
        ),
    ] {
        let executable = work.join(format!("oracle-{mode}"));
        oracle::compile_cpp_with_flags(
            &include,
            &executable,
            &[
                root.join("tests/segmented_vector_oracle.cpp"),
                root.join("tests/segmented-reference/tpde/src/util/SmallVector.cpp"),
            ],
            assertions,
            flags,
        );
        for (idx, (input, expected, _)) in cases.iter().enumerate() {
            oracle::compare_bytes(
                &executable,
                &work,
                input,
                expected,
                &format!("segmented-{idx}-{mode}"),
            );
        }
        if mode != "sanitizers" {
            rejected_cpp_forms(&include, &work, assertions);
        }
    }
    fs::remove_dir_all(work).unwrap();
    println!("TPDE_SEGMENTED_VECTOR_ORACLE PASS segment_sizes=3 cpp_modes=3 operations={} negative_forms=8", operations * 3);
}

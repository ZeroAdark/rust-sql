// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![cfg(feature = "upstream-oracle")]

#[path = "../../test-support/oracle.rs"]
mod oracle;

use core::cell::{RefCell, RefMut};
use std::{fs, path::Path};
use tpde_util::function_ref::{FunctionRef, FunctionRefMut};

fn emit(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn affine(value: u64) -> u64 {
    value.wrapping_mul(3).wrapping_add(7)
}

fn alternate(value: u64) -> u64 {
    (value ^ 0x938fad08533d5821).wrapping_mul(5)
}

fn identity(value: &u64) -> &u64 {
    value
}

fn cases() -> (Vec<u8>, Vec<u8>, usize) {
    let mut seeds = vec![0, 1, u64::MAX, i64::MIN as u64, i64::MAX as u64];
    let mut state = 0x2d930a057f21b689u64;
    for _ in 0..512 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        seeds.push(state);
    }
    let mut input = (seeds.len() as u32).to_le_bytes().to_vec();
    let mut output = Vec::new();
    for &seed in &seeds {
        emit(&mut input, seed);
        let salt = seed.rotate_left(7);
        let pointer: fn(u64) -> u64 = affine;
        let callback = FunctionRef::new(&pointer);
        emit(&mut output, callback(seed));
        let callable = |value: u64| value ^ salt;
        let callback = FunctionRef::new(&callable);
        let copy = callback;
        emit(&mut output, copy(seed));
        let callable = |a: u64, b: u64| a.wrapping_mul(b).wrapping_add(salt);
        emit(&mut output, FunctionRef::new(&callable)(seed, 17));
        let callable = |a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64, h: u64, i: u64| {
            a.wrapping_add(b)
                .wrapping_add(c)
                .wrapping_add(d)
                .wrapping_add(e)
                .wrapping_add(f)
                .wrapping_add(g)
                .wrapping_add(h)
                .wrapping_add(i)
        };
        emit(
            &mut output,
            FunctionRef::new(&callable)(seed, 1, 2, 3, 4, 5, 6, 7, 8),
        );
        let callable = || seed;
        emit(&mut output, FunctionRef::new(&callable)());
        let callback = FunctionRef::new(&identity);
        emit(&mut output, *callback(&seed));
        emit(&mut output, core::ptr::eq(callback(&seed), &seed) as u64);
        let callable = |value: &mut u64| *value ^= salt;
        let mut value = seed;
        FunctionRef::new(&callable)(&mut value);
        emit(&mut output, value);
        let callable = |value: Box<u64>| value.wrapping_add(salt);
        emit(&mut output, FunctionRef::new(&callable)(Box::new(seed)));
        let mut state = seed;
        let callable = RefCell::new(move |value| {
            state = state.wrapping_add(value);
            state
        });
        let callback = FunctionRefMut::new(&callable);
        let copy = callback;
        emit(&mut output, callback.with_mut(|f| f(1)));
        emit(&mut output, copy.with_mut(|f| f(salt)));
        emit(&mut output, copy.try_with_mut(|f| f(seed)).unwrap());
        struct Accessor(u64);
        impl Accessor {
            fn get(&mut self) -> &mut u64 {
                &mut self.0
            }
        }
        let callable = RefCell::new(Accessor(seed));
        let callback = FunctionRefMut::new(&callable);
        let copy = callback;
        let first_address;
        {
            let mut reference = RefMut::map(callback.borrow_mut(), Accessor::get);
            *reference ^= salt;
            first_address = &*reference as *const u64;
            emit(&mut output, *reference);
        }
        {
            let mut reference = RefMut::map(copy.borrow_mut(), Accessor::get);
            *reference = reference.wrapping_add(1);
            emit(&mut output, *reference);
            emit(
                &mut output,
                core::ptr::eq(first_address, &*reference) as u64,
            );
        }
        let mut external = seed;
        let callable = RefCell::new(|value: u64| external ^= value);
        let callback = FunctionRefMut::new(&callable);
        let copy = callback;
        callback.with_mut(|f| f(salt));
        copy.with_mut(|f| f(17));
        drop(callable);
        emit(&mut output, external);
        let first_pointer: fn(u64) -> u64 = affine;
        let second_pointer: fn(u64) -> u64 = alternate;
        let original = FunctionRef::new(&first_pointer);
        let nested = FunctionRef::new(&original);
        emit(&mut output, nested(seed));
        let original = RefCell::new(original);
        let snapshot = *original.borrow();
        let forwarded = FunctionRefMut::new(&original);
        emit(&mut output, forwarded.with_mut(|f| f(seed)));
        *original.borrow_mut() = FunctionRef::new(&second_pointer);
        emit(&mut output, forwarded.with_mut(|f| f(seed)));
        emit(&mut output, snapshot(seed));
        emit(
            &mut output,
            FunctionRef::new(&FunctionRef::new(&first_pointer))(seed),
        );
    }
    assert_eq!(seeds.len(), 517);
    assert_eq!(output.len(), seeds.len() * 21 * 8);
    (input, output, seeds.len())
}

#[test]
fn complete_function_ref_matches_pinned_cpp() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let work = std::env::temp_dir().join(format!("tpde-function-ref-{}", std::process::id()));
    fs::create_dir_all(&work).unwrap();
    let (input, expected, cases) = cases();
    for assertions in [true, false] {
        let executable = work.join(if assertions {
            "oracle-debug"
        } else {
            "oracle-release"
        });
        oracle::compile_cpp(
            &root.join("tests/function_ref_reference/tpde/include"),
            &executable,
            &[root.join("tests/function_ref_oracle.cpp")],
            assertions,
        );
        oracle::compare_bytes(
            &executable,
            &work,
            &input,
            &expected,
            "function_ref signatures/copies",
        );
    }
    eprintln!("TPDE_FUNCTION_REF_ORACLE PASS cases={cases} cpp_modes=2 outputs_per_case=21");
    fs::remove_dir_all(work).unwrap();
}

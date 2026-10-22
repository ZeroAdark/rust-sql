// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use mcx::{Mcx, MemoryContext};
use std::rc::Rc;
use tpde_util::{SmallVec, SmallVectorGuard};
#[path = "small_vector/tracked.rs"]
mod tracked;
use tracked::*;
#[path = "small_vector/memory.rs"]
mod memory;
#[path = "small_vector/unit.rs"]
mod unit;

#[cfg(feature = "upstream-oracle")]
#[path = "../../test-support/oracle.rs"]
mod oracle;

#[cfg(feature = "upstream-oracle")]
mod differential {
    use super::*;
    use std::{
        fs,
        ops::{Deref, DerefMut},
        path::Path,
        process::Command,
    };

    fn out(dst: &mut Vec<u8>, value: u64) {
        dst.extend_from_slice(&value.to_le_bytes());
    }
    trait Element: Clone + Default {
        fn make(value: i64) -> Self;
        fn value(&self) -> i64;
        fn set(&mut self, value: i64);
    }
    impl Element for i64 {
        fn make(value: i64) -> Self {
            value
        }
        fn value(&self) -> i64 {
            *self
        }
        fn set(&mut self, value: i64) {
            *self = value;
        }
    }
    impl Element for Tracked {
        fn make(value: i64) -> Self {
            Self::new(value)
        }
        fn value(&self) -> i64 {
            *self.resource
        }
        fn set(&mut self, value: i64) {
            *self.resource = value;
        }
    }
    trait Uninitialized {
        fn fill_uninitialized(&mut self, len: usize, value: i64);
    }
    impl<const N: usize> Uninitialized for SmallVec<'_, i64, N> {
        fn fill_uninitialized(&mut self, len: usize, value: i64) {
            let mut suffix = self.resize_uninitialized(len).unwrap();
            for (i, slot) in suffix.iter_mut().enumerate() {
                slot.write(value.wrapping_add(i as i64));
            }
            // SAFETY: the loop initialized each exposed suffix slot.
            unsafe {
                suffix.assume_init();
            }
        }
    }
    impl<const N: usize> Uninitialized for SmallVectorGuard<'_, Tracked, N> {
        fn fill_uninitialized(&mut self, len: usize, value: i64) {
            let old = self.len();
            self.resize_default(len).unwrap();
            for (i, item) in self.iter_mut().skip(old).enumerate() {
                item.set(value.wrapping_add(i as i64));
            }
        }
    }
    trait SelectedForward<E> {
        fn append_selected(&mut self, items: &[E]);
    }
    impl<const N: usize> SelectedForward<i64> for SmallVec<'_, i64, N> {
        fn append_selected(&mut self, items: &[i64]) {
            self.append_forward(items.iter().copied().filter(|v| v & 1 == 0))
                .unwrap();
        }
    }
    impl<const N: usize> SelectedForward<Tracked> for SmallVectorGuard<'_, Tracked, N> {
        fn append_selected(&mut self, items: &[Tracked]) {
            self.append_cloned_forward(items.iter().filter(|v| v.value() & 1 == 0))
                .unwrap();
        }
    }
    trait Sequence<'m, E: Element>: Deref<Target = [E]> + DerefMut + Sized {
        fn empty(mcx: Mcx<'m>) -> Self;
        fn capacity(&self) -> usize;
        fn max_size() -> usize;
        fn operation(&mut self, op: u8, n: usize, m: usize, value: i64, output: &mut Vec<u8>);
        fn transfer(&mut self, other: &mut Self);
        fn moved(mcx: Mcx<'m>, other: &mut Self) -> Self;
        fn allocation_failure(&mut self) -> bool;
    }
    macro_rules! sequence {
        ($vector:ident, $element:ty) => {
            impl<'m, const N: usize> Sequence<'m, $element> for $vector<'m, $element, N> {
                fn empty(mcx: Mcx<'m>) -> Self {
                    Self::new(mcx)
                }
                fn capacity(&self) -> usize {
                    self.capacity()
                }
                fn max_size() -> usize {
                    Self::max_size()
                }
                fn transfer(&mut self, other: &mut Self) {
                    self.move_assign(other).unwrap();
                }
                fn moved(mcx: Mcx<'m>, other: &mut Self) -> Self {
                    Self::move_from(mcx, other).unwrap()
                }
                fn allocation_failure(&mut self) -> bool {
                    self.reserve(isize::MAX as usize / core::mem::size_of::<$element>())
                        .is_err()
                }
                fn operation(
                    &mut self,
                    op: u8,
                    n: usize,
                    m: usize,
                    value: i64,
                    output: &mut Vec<u8>,
                ) {
                    match op {
                        0 => {}
                        1 => self.clear(),
                        2 => self.reserve(n).unwrap(),
                        3 => self.push(<$element>::make(value)).unwrap(),
                        4 => self.push_clone(&<$element>::make(value)).unwrap(),
                        5 => self
                            .emplace_with(|| <$element>::make(value))
                            .unwrap()
                            .set(value ^ 0x5678),
                        6 => self.pop_back().unwrap(),
                        7 => self.resize(n, <$element>::make(value)).unwrap(),
                        8 => self.resize_default(n).unwrap(),
                        9 => out(output, self.erase(n..m).unwrap() as u64),
                        10 | 11 | 17 | 18 => {
                            let items: Vec<_> =
                                (0..n)
                                    .map(|i| {
                                        <$element>::make((value as u64).wrapping_add(
                                            (i as u64).wrapping_mul(0x9e3779b97f4a7c15),
                                        ) as i64)
                                    })
                                    .collect();
                            match op {
                                10 => self.append(items.into_iter()).unwrap(),
                                11 => self.append_cloned(items.iter()).unwrap(),
                                17 => self.append_selected(&items),
                                18 => self
                                    .append_cloned_forward(
                                        items.iter().filter(|v| v.value() & 1 == 0),
                                    )
                                    .unwrap(),
                                _ => unreachable!(),
                            }
                        }
                        14 => self[n].set(value),
                        15 => self.fill_uninitialized(n, value),
                        16 => out(
                            output,
                            self.pop().map_or(u64::MAX, |item| item.value() as u64),
                        ),
                        19 => self.release(),
                        _ => unreachable!(),
                    }
                }
            }
        };
    }
    sequence!(SmallVec, i64);
    sequence!(SmallVectorGuard, Tracked);

    fn state<'m, E: Element, V: Sequence<'m, E>>(expected: &mut Vec<u8>, v: &V) {
        out(expected, v.len() as u64);
        out(expected, v.capacity() as u64);
        out(expected, v.is_empty() as u64);
        out(expected, V::max_size() as u64);
        out(expected, v.first().map_or(u64::MAX, |v| v.value() as u64));
        out(expected, v.last().map_or(u64::MAX, |v| v.value() as u64));
        out(expected, v.iter().len() as u64);
        out(expected, v.iter().len() as u64);
        out(expected, 1);
        out(expected, 1);
        for index in 0..v.len() {
            out(expected, v[index].value() as u64);
        }
        for item in v.iter() {
            out(expected, item.value() as u64);
        }
        for item in v.iter().rev() {
            out(expected, item.value() as u64);
        }
    }
    struct Script<'m, E: Element, V: Sequence<'m, E>> {
        vectors: [V; 2],
        mcx: Mcx<'m>,
        input: Vec<u8>,
        expected: Vec<u8>,
        operations: u32,
        stats: Rc<Stats>,
        element: core::marker::PhantomData<E>,
    }
    impl<'m, E: Element, V: Sequence<'m, E>> Script<'m, E, V> {
        fn new(mcx: Mcx<'m>, kind: u8, inline: u8, stats: Rc<Stats>) -> Self {
            let mut result = Self {
                vectors: core::array::from_fn(|_| V::empty(mcx)),
                mcx,
                input: vec![kind, inline, 0, 0, 0, 0],
                expected: Vec::new(),
                operations: 0,
                stats,
                element: core::marker::PhantomData,
            };
            result.snapshot();
            result
        }
        fn snapshot(&mut self) {
            for v in &self.vectors {
                state::<E, V>(&mut self.expected, v);
            }
            out(&mut self.expected, self.stats.created.get() as u64);
            out(&mut self.expected, self.stats.released.get() as u64);
        }
        fn op(&mut self, op: u8, a: usize, b: usize, n: usize, m: usize, value: i64) {
            self.input.extend_from_slice(&[op, a as u8, b as u8, 0]);
            self.input.extend_from_slice(&(n as u32).to_le_bytes());
            self.input.extend_from_slice(&(m as u32).to_le_bytes());
            out(&mut self.input, value as u64);
            match op {
                12 => {
                    let (left, right) = self.vectors.split_at_mut(1);
                    if a == 0 {
                        left[0].transfer(&mut right[0]);
                    } else {
                        right[0].transfer(&mut left[0]);
                    }
                }
                13 => self.vectors[a] = V::moved(self.mcx, &mut self.vectors[b]),
                _ => self.vectors[a].operation(op, n, m, value, &mut self.expected),
            }
            self.operations += 1;
            self.snapshot();
        }
        fn simple(&mut self, op: u8, n: usize, m: usize, value: i64) {
            self.op(op, 0, 0, n, m, value);
        }
        fn run(mut self) -> (Vec<u8>, Vec<u8>, u32) {
            for n in [0, 1, 2, 3, 4, 5, 8, 9, 17, 31, 32, 33, 63, 64, 65] {
                self.simple(1, 0, 0, 0);
                self.simple(2, n, 0, 0);
                self.simple(7, n, 0, -9);
                self.simple(4, 0, 0, i64::MIN);
                self.simple(3, 0, 0, i64::MAX);
                self.simple(5, 0, 0, 0x1234);
                self.simple(14, n, 0, 55);
                self.simple(9, 1, 3, 0);
                self.simple(9, 0, 0, 0);
                self.simple(6, 0, 0, 0);
                self.simple(8, n + 5, 0, 0);
                self.simple(15, n + 8, 0, 71);
                self.simple(10, 3, 0, 1);
                self.simple(11, 4, 0, 2);
                self.simple(17, 5, 0, 3);
                self.simple(18, 6, 0, 4);
                self.simple(8, 0, 0, 0);
                self.simple(16, 0, 0, 0);
            }
            let mut random = 0x13c8a2b07451c69du64;
            for _ in 0..256 {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                let a = (random & 1) as usize;
                let len = self.vectors[a].len();
                let n = ((random >> 16) % 80) as usize;
                let value = (random >> 1) as i64;
                match (random >> 8) % 12 {
                    0 => self.op(1, a, 0, 0, 0, value),
                    1 => self.op(2, a, 0, n, 0, value),
                    2 => self.op(3, a, 0, 0, 0, value),
                    3 => self.op(4, a, 0, 0, 0, value),
                    4 => self.op(7, a, 0, n, 0, value),
                    5 => self.op(8, a, 0, n, 0, value),
                    6 => self.op(9, a, 0, len / 3, len * 2 / 3, value),
                    7 => self.op(10, a, 0, n % 9, 0, value),
                    8 => self.op(11, a, 0, n % 9, 0, value),
                    9 => self.op(12, a, 1 - a, 0, 0, value),
                    10 => self.op(13, a, 1 - a, 0, 0, value),
                    _ => self.op(16, a, 0, 0, 0, value),
                }
            }
            for source in [0, 1, 4, 5, 17] {
                for destination in [0, 1, 4, 9] {
                    for op in [12, 13] {
                        self.op(19, 0, 0, 0, 0, 0);
                        self.op(19, 1, 0, 0, 0, 0);
                        self.op(8, 0, 0, source, 0, 0);
                        self.op(8, 1, 0, destination, 0, 0);
                        self.op(op, 1, 0, 0, 0, 0);
                        self.op(3, 0, 0, 0, 0, 77);
                    }
                }
            }
            self.input[2..6].copy_from_slice(&self.operations.to_le_bytes());
            drop(self.vectors);
            out(&mut self.expected, self.stats.created.get() as u64);
            out(&mut self.expected, self.stats.released.get() as u64);
            (self.input, self.expected, self.operations)
        }
    }
    fn case<const N: usize>(tracked: bool) -> (Vec<u8>, Vec<u8>, u32) {
        let mut result = None;
        tracked_scope(|stats| {
            let context = MemoryContext::new("vector-oracle");
            result = Some(if tracked {
                Script::<Tracked, SmallVectorGuard<'_, Tracked, N>>::new(
                    context.mcx(),
                    1,
                    N as u8,
                    stats.clone(),
                )
                .run()
            } else {
                Script::<i64, SmallVec<'_, i64, N>>::new(context.mcx(), 0, N as u8, stats.clone())
                    .run()
            });
        });
        result.unwrap()
    }
    fn dimensions() -> Vec<u8> {
        let mut result = Vec::new();
        for size in [
            tpde_util::default_inline_capacity::<u8>(),
            tpde_util::default_inline_capacity::<u64>(),
            tpde_util::default_inline_capacity::<[u8; 255]>(),
            tpde_util::default_inline_capacity::<[u8; 256]>(),
            tpde_util::default_inline_capacity::<[u8; 257]>(),
            SmallVec::<u8, 0>::max_size(),
            SmallVec::<u64, 0>::max_size(),
            SmallVec::<[u8; 257], 0>::max_size(),
        ] {
            out(&mut result, size as u64);
        }
        result
    }
    fn cross<const A: usize, const B: usize>(expected: &mut Vec<u8>) {
        let context = MemoryContext::new_bump("vector-cross");
        let mut a = SmallVec::<i64, A>::new(context.mcx());
        let mut b = SmallVec::<i64, B>::new(context.mcx());
        for count in [0, 1, 4, 5, 17] {
            a.release();
            b.release();
            for i in 0..count {
                b.push(i * 19).unwrap();
            }
            a.move_assign(&mut b).unwrap();
            state::<i64, _>(expected, &a);
            state::<i64, _>(expected, &b);
            b.push(-9).unwrap();
            let c = SmallVec::<i64, B>::move_from(context.mcx(), &mut a).unwrap();
            state::<i64, _>(expected, &a);
            state::<i64, _>(expected, &b);
            state::<i64, _>(expected, &c);
            a.push(-11).unwrap();
            state::<i64, _>(expected, &a);
        }
    }
    fn allocation_failure<'m, E: Element, V: Sequence<'m, E>>(
        mcx: Mcx<'m>,
        expected: &mut Vec<u8>,
        stats: &Rc<Stats>,
        heap: bool,
    ) {
        let mut v = V::empty(mcx);
        v.operation(3, 0, 0, 7, expected);
        if heap {
            v.operation(2, 8, 0, 0, expected);
        }
        state::<E, V>(expected, &v);
        out(expected, v.allocation_failure() as u64);
        state::<E, V>(expected, &v);
        out(expected, stats.created.get() as u64);
        out(expected, stats.released.get() as u64);
        drop(v);
        out(expected, stats.created.get() as u64);
        out(expected, stats.released.get() as u64);
    }
    fn allocation_failures() -> Vec<u8> {
        let mut expected = Vec::new();
        for tracked in [false, true] {
            for heap in [false, true] {
                tracked_scope(|stats| {
                    let context = MemoryContext::new("vector-failure").with_limit(8192);
                    if tracked {
                        allocation_failure::<Tracked, SmallVectorGuard<'_, Tracked, 4>>(
                            context.mcx(),
                            &mut expected,
                            stats,
                            heap,
                        );
                    } else {
                        allocation_failure::<i64, SmallVec<'_, i64, 4>>(
                            context.mcx(),
                            &mut expected,
                            stats,
                            heap,
                        );
                    }
                });
            }
        }
        expected
    }
    fn forward_move() -> Vec<u8> {
        struct MoveOnly(Tracked);
        let mut expected = Vec::new();
        tracked_scope(|stats| {
            let context = MemoryContext::new("vector-forward-move");
            {
                let source: [std::cell::Cell<Option<MoveOnly>>; 5] =
                    core::array::from_fn(|index| {
                        std::cell::Cell::new(Some(MoveOnly(Tracked::new(index as i64 + 10))))
                    });
                let mut destination = SmallVectorGuard::<MoveOnly, 1>::new(context.mcx());
                let mut calls = 0;
                stats.clone_budget.set(Some(0));
                destination
                    .append_forward_with(
                        source
                            .iter()
                            .enumerate()
                            .filter(|(index, _)| index % 2 == 0),
                        |(_, slot)| {
                            calls += 1;
                            slot.take().unwrap()
                        },
                    )
                    .unwrap();
                assert_eq!(calls, 3);
                out(&mut expected, destination.len() as u64);
                out(&mut expected, destination.capacity() as u64);
                for item in destination.iter() {
                    out(&mut expected, *item.0.resource as u64);
                }
                for slot in &source {
                    let item = slot.take();
                    out(&mut expected, item.is_some() as u64);
                    out(
                        &mut expected,
                        item.as_ref()
                            .map_or(u64::MAX, |item| *item.0.resource as u64),
                    );
                    slot.set(item);
                }
                out(&mut expected, stats.created.get() as u64);
                out(&mut expected, stats.released.get() as u64);
                destination.clear();
                out(&mut expected, stats.created.get() as u64);
                out(&mut expected, stats.released.get() as u64);
            }
            out(&mut expected, stats.created.get() as u64);
            out(&mut expected, stats.released.get() as u64);
        });
        expected
    }
    fn rejected_forms(include: &Path, work: &Path, assertions: bool) {
        for (name, declaration, diagnostic) in [
            (
                "copy-ctor",
                "tpde::util::SmallVector<int> v; auto copied = v;",
                "deleted",
            ),
            (
                "copy-assign",
                "tpde::util::SmallVector<int> a, b; a = b;",
                "deleted",
            ),
            (
                "overaligned",
                "struct alignas(64) Aligned { int v; }; tpde::util::SmallVector<Aligned> v;",
                "static assertion",
            ),
        ] {
            let source = work.join(format!("{name}.cpp"));
            fs::write(&source, format!("#include <cstdlib>\n#include \"tpde/util/SmallVector.hpp\"\nvoid f() {{ {declaration} }}\n")).unwrap();
            let mut command =
                Command::new(std::env::var("CXX").unwrap_or_else(|_| "clang++".into()));
            command
                .args(["-std=c++20", "-fsyntax-only", "-I"])
                .arg(include)
                .arg(source);
            if !assertions {
                command.arg("-DNDEBUG");
            }
            let output = command.output().unwrap();
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(
                !output.status.success() && error.contains(diagnostic),
                "{name}: wrong failure: {error}"
            );
        }
    }
    #[test]
    fn complete_small_vector_matches_pinned_cpp() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let include = root.join("tests/vector-reference/tpde/include");
        let work = std::env::temp_dir().join(format!("tpde-small-vector-{}", std::process::id()));
        fs::create_dir_all(&work).unwrap();
        let cases = [
            case::<0>(false),
            case::<1>(false),
            case::<4>(false),
            case::<32>(false),
            case::<0>(true),
            case::<1>(true),
            case::<4>(true),
            case::<32>(true),
        ];
        let operations: u32 = cases.iter().map(|c| c.2).sum();
        let mut crosses = Vec::new();
        cross::<0, 4>(&mut crosses);
        cross::<4, 0>(&mut crosses);
        cross::<1, 32>(&mut crosses);
        cross::<32, 1>(&mut crosses);
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
                    root.join("tests/small_vector_oracle.cpp"),
                    root.join("tests/vector-reference/tpde/src/util/SmallVector.cpp"),
                    root.join("tests/vector-reference/tpde/src/base.cpp"),
                ],
                assertions,
            );
            oracle::compare_bytes(
                &executable,
                &work,
                &[2],
                &dimensions(),
                "vector constants and type forms",
            );
            oracle::compare_bytes(
                &executable,
                &work,
                &[3],
                &crosses,
                "cross-inline move constructors and assignments",
            );
            oracle::compare_bytes(
                &executable,
                &work,
                &[4],
                &allocation_failures(),
                "trivial and nontrivial allocation failures from inline and heap storage",
            );
            oracle::compare_bytes(
                &executable,
                &work,
                &[5],
                &forward_move(),
                "move-only resources through a filtered forward iterator",
            );
            for (index, (input, expected, _)) in cases.iter().enumerate() {
                oracle::compare_bytes(
                    &executable,
                    &work,
                    input,
                    expected,
                    &format!("vector script {index}"),
                );
            }
            rejected_forms(&include, &work, assertions);
        }
        assert!(operations >= 2000);
        eprintln!("TPDE_SMALL_VECTOR_ORACLE PASS scripts=8 cpp_modes=2 operations={operations} resource_classes=2 cross_moves=20 allocation_failures=4 forward_moves=1");
        fs::remove_dir_all(work).unwrap();
    }
}

// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#[path = "../../test-support/oracle.rs"]
mod oracle;
use std::{collections::BTreeMap, fmt::Write as _, fs, path::Path, process::Command};
use tpde_util::{misc::*, *};
fn out(dst: &mut Vec<u8>, v: u64) {
    dst.extend_from_slice(&v.to_le_bytes());
}
fn u32in(dst: &mut Vec<u8>, v: usize) {
    dst.extend_from_slice(&(v as u32).to_le_bytes());
}
fn numeric() -> (Vec<u8>, Vec<u8>, usize) {
    let mut values = vec![0, 1, u64::MAX, i64::MAX as u64, i64::MIN as u64];
    for i in 0..64 {
        let v = 1u64 << i;
        values.extend([v - 1, v, v.saturating_add(1)]);
    }
    let mut state = 0x832ac691bc34d875u64;
    for _ in 0..512 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        values.push(state);
    }
    let mut input = vec![0];
    u32in(&mut input, values.len());
    let mut expected = Vec::new();
    for (i, &v) in values.iter().enumerate() {
        let bits = (i % 63 + 1) as u32;
        out(&mut input, v);
        input.push(bits as u8);
        macro_rules! unsigned {
            ($t:ty) => {{
                let value = (v as $t) | 1;
                out(&mut expected, cnt_tz(value).unwrap() as u64);
                out(&mut expected, cnt_lz(value).unwrap() as u64);
                let value = (v & ((<$t>::MAX >> 1) as u64)) as $t;
                out(&mut expected, align_down(value, 8).unwrap() as u64);
                out(&mut expected, align_up(value, 8).unwrap() as u64);
            }};
        }
        unsigned!(u8);
        unsigned!(u16);
        unsigned!(u32);
        unsigned!(u64);
        macro_rules! signed {
            ($t:ty) => {{
                let value = (v & 63) as $t - 31;
                out(&mut expected, align_down(value, 8).unwrap() as u64);
                out(&mut expected, align_up(value, 8).unwrap() as u64);
            }};
        }
        signed!(i8);
        signed!(i16);
        signed!(i32);
        signed!(i64);
        out(&mut expected, zext(v, bits).unwrap());
        out(&mut expected, sext(v, bits).unwrap() as u64);
        let mut bytes = [0; 10];
        let n = uleb_write(&mut bytes, v).unwrap();
        out(&mut expected, uleb_len(v) as u64);
        out(&mut expected, n as u64);
        expected.extend_from_slice(&bytes[..n]);
        let n = sleb_write(&mut bytes, v as i64).unwrap();
        out(&mut expected, n as u64);
        expected.extend_from_slice(&bytes[..n]);
        out(&mut expected, v.count_ones() as u64);
        for bit in BitSetIterator::<false>::new(v) {
            out(&mut expected, bit);
        }
        out(&mut expected, v.count_ones() as u64);
        for bit in BitSetIterator::<true>::new(v) {
            out(&mut expected, bit);
        }
    }
    assert_eq!(values.len(), 709);
    (input, expected, values.len())
}
fn integral(w: &mut VectorWriter<'_, '_, 8>, kind: u8, v: u64, checked: bool) {
    macro_rules! write {
        ($v:expr) => {
            if checked {
                w.write_int($v).unwrap()
            } else {
                w.write_int_unchecked($v).unwrap()
            }
        };
    }
    match kind {
        0 => write!(v != 0),
        1 => write!(v as u8),
        2 => write!(v as u16),
        3 => write!(v as u32),
        4 => write!(v),
        5 => write!(v as usize),
        6 => write!(v as i8),
        7 => write!(v as i16),
        8 => write!(v as i32),
        9 => write!(v as i64),
        10 => write!(v as isize),
        _ => unreachable!(),
    }
}
struct Script<'a, 'm> {
    w: VectorWriter<'a, 'm, 8>,
    input: Vec<u8>,
    expected: Vec<u8>,
    operations: usize,
}
impl Script<'_, '_> {
    fn snapshot(&mut self) {
        out(&mut self.expected, self.w.size() as u64);
        out(&mut self.expected, self.w.capacity() as u64);
        self.expected
            .extend_from_slice(&self.w.data().unwrap()[..self.w.size()]);
        self.operations += 1;
    }
    fn size_op(&mut self, op: u8, n: usize) {
        self.input.push(op);
        u32in(&mut self.input, n);
        match op {
            1 => self.w.reserve(n).unwrap(),
            3 => self.w.zero(n).unwrap(),
            5 => self.w.unskip(n).unwrap(),
            11 => self.w.zero_unchecked(n).unwrap(),
            17 => self.w.skip_unchecked(n).unwrap(),
            4 | 12 => {
                let off = self.w.size();
                if op == 4 {
                    self.w.skip(n).unwrap()
                } else {
                    self.w.skip_unchecked(n).unwrap()
                }
                self.w.data_mut().unwrap()[off..off + n].fill(0x5a);
            }
            _ => unreachable!(),
        }
        self.snapshot();
    }
    fn write(&mut self, op: u8, bytes: &[u8]) {
        self.input.push(op);
        u32in(&mut self.input, bytes.len());
        self.input.extend_from_slice(bytes);
        if op == 2 {
            self.w.write(bytes).unwrap()
        } else {
            self.w.write_unchecked(bytes).unwrap()
        }
        self.snapshot();
    }
    fn value(&mut self, op: u8, v: u64) {
        self.input.push(op);
        out(&mut self.input, v);
        match op {
            8 => self.w.write_uleb(v).unwrap(),
            9 => self.w.write_sleb(v as i64).unwrap(),
            13 => self.w.write_uleb_unchecked(v).unwrap(),
            14 => self.w.write_sleb_unchecked(v as i64).unwrap(),
            _ => unreachable!(),
        }
        self.snapshot();
    }
    fn integer(&mut self, kind: u8, v: u64, checked: bool) {
        self.input.push(if checked { 7 } else { 18 });
        self.input.push(kind);
        out(&mut self.input, v);
        integral(&mut self.w, kind, v, checked);
        self.snapshot();
    }
    fn flush(&mut self) {
        self.input.push(6);
        self.w.flush();
        self.snapshot();
    }
    fn move_roundtrip(&mut self) {
        self.input.push(15);
        let moved = core::mem::take(&mut self.w);
        self.w = moved;
        self.snapshot();
    }
}
fn writer(seed: usize) -> (Vec<u8>, Vec<u8>, usize) {
    let context = mcx::MemoryContext::new_bump("writer-diff");
    let mut old = SmallVec::<u8, 8>::new(context.mcx());
    old.resize(8, 0x33).unwrap();
    let mut v = SmallVec::<u8, 8>::new(context.mcx());
    let initial: Vec<_> = (0..[0, 1, 8, 9, 17, 33, 256, 257][seed])
        .map(|i| (i * 73) as u8)
        .collect();
    v.extend_from_slice(&initial).unwrap();
    let offset = if seed % 2 == 0 {
        u32::MAX as usize
    } else {
        initial.len() / 2
    };
    let mut input = vec![1];
    u32in(&mut input, initial.len());
    input.extend_from_slice(&initial);
    u32in(&mut input, offset);
    let (input, mut expected, operations) = {
        let w = if offset == u32::MAX as usize {
            VectorWriter::new(&mut v)
        } else {
            VectorWriter::at(&mut v, offset).unwrap()
        };
        let mut s = Script {
            w,
            input,
            expected: Vec::new(),
            operations: 0,
        };
        if offset != u32::MAX as usize {
            s.size_op(17, initial.len() - offset);
        }
        for i in 0..24usize {
            let n = [0, 1, 7, 8, 9, 16, 17, 255, 256, 257][(i + seed) % 10];
            let data: Vec<_> = (0..n).map(|j| (j * 131 + i + seed) as u8).collect();
            s.size_op(1, n + 32);
            s.write(10, &data);
            s.size_op(5, n.min(3));
            s.write(2, b"\0\xff");
            s.size_op(4, i % 5);
            s.size_op(3, i % 7);
            s.size_op(1, 32);
            s.size_op(12, 3);
            s.size_op(11, 2);
            let value = [0, 127, 128, u64::MAX, i64::MIN as u64, i64::MAX as u64][i % 6];
            s.value(8, value);
            s.value(9, value);
            s.size_op(1, 32);
            s.value(13, value);
            s.value(14, value);
            s.integer((i % 11) as u8, value, true);
            s.size_op(1, 8);
            s.integer((i % 11) as u8, value, false);
            s.move_roundtrip();
            s.flush();
        }
        s.input.push(16);
        {
            let mut destination = VectorWriter::new(&mut old);
            destination.unskip(3).unwrap();
            destination = core::mem::take(&mut s.w);
            s.w = core::mem::take(&mut destination);
        }
        s.snapshot();
        s.size_op(5, 1);
        s.input.push(255);
        let input = core::mem::take(&mut s.input);
        let expected = core::mem::take(&mut s.expected);
        let operations = s.operations;
        drop(s);
        (input, expected, operations)
    };
    out(&mut expected, v.len() as u64);
    out(&mut expected, v.capacity() as u64);
    expected.extend_from_slice(&v);
    assert_eq!(&*old, &[0x33; 5]);
    out(&mut expected, old.len() as u64);
    expected.extend_from_slice(&old);
    (input, expected, operations)
}
fn constants(root: &Path, work: &Path) -> usize {
    let mut expected = BTreeMap::new();
    for (namespace, items) in [
        ("", dwarf::CONSTANTS),
        ("x64::", dwarf::x64::CONSTANTS),
        ("a64::", dwarf::a64::CONSTANTS),
    ] {
        for &(name, value) in items {
            expected.insert(format!("{namespace}{name}"), value);
        }
    }
    let header =
        fs::read_to_string(root.join("tests/upstream/tpde/include/tpde/DWARF.hpp")).unwrap();
    let mut namespace = "";
    let mut found = std::collections::BTreeSet::new();
    let mut source =
        String::from("#include <iostream>\n#include \"tpde/DWARF.hpp\"\nint main(){\n");
    let mut output = String::new();
    for line in header.lines().map(str::trim) {
        if line == "namespace x64 {" {
            namespace = "x64::";
        } else if line == "namespace a64 {" {
            namespace = "a64::";
        } else if line.starts_with("} // namespace") {
            namespace = "";
        }
        if line.starts_with("constexpr ") {
            let fields: Vec<_> = line.split_whitespace().collect();
            let name = format!("{namespace}{}", fields[2]);
            let value = expected
                .get(&name)
                .unwrap_or_else(|| panic!("missing {name}"));
            assert!(found.insert(name.clone()));
            writeln!(
                source,
                "std::cout << \"{name} \" << +tpde::dwarf::{name} << \"\\n\";"
            )
            .unwrap();
            writeln!(output, "{name} {value}").unwrap();
            let width = if fields[1] == "u8" { 1 } else { 4 };
            writeln!(
                source,
                "static_assert(sizeof(tpde::dwarf::{name})=={width});"
            )
            .unwrap();
        }
    }
    assert_eq!(found.len(), expected.len());
    source.push_str("}\n");
    let file = work.join("dwarf.cpp");
    fs::write(&file, source).unwrap();
    let exe = work.join("dwarf");
    oracle::compile_cpp(
        &root.join("tests/upstream/tpde/include"),
        &exe,
        &[file],
        true,
    );
    let result = Command::new(exe).output().unwrap();
    assert!(result.status.success());
    assert_eq!(result.stdout, output.as_bytes());
    found.len()
}
#[test]
fn complete_utilities_match_pinned_cpp() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let work = std::env::temp_dir().join(format!("tpde-util-oracle-{}", std::process::id()));
    fs::create_dir_all(&work).unwrap();
    let count = constants(root, &work);
    let numeric = numeric();
    let mut operations = 0;
    for assertions in [false, true] {
        let exe = work.join(if assertions {
            "oracle-debug"
        } else {
            "oracle-release"
        });
        oracle::compile_cpp(
            &root.join("tests/upstream/tpde/include"),
            &exe,
            &[
                root.join("tests/oracle.cpp"),
                root.join("tests/upstream/tpde/src/util/SmallVector.cpp"),
            ],
            assertions,
        );
        oracle::compare_bytes(
            &exe,
            &work,
            &numeric.0,
            &numeric.1,
            "numeric boundary cases",
        );
        for seed in 0..8 {
            let (input, expected, n) = writer(seed);
            oracle::compare_bytes(
                &exe,
                &work,
                &input,
                &expected,
                &format!("writer seed {seed}"),
            );
            operations += n;
        }
    }
    assert!(operations > 7000);
    assert!(count > 100);
    eprintln!(
        "TPDE_UTIL_ORACLE PASS numeric_cases={} writer_scripts=8 cpp_modes=2 operations={operations} dwarf_constants={count}",
        numeric.2
    );
    fs::remove_dir_all(work).unwrap();
}

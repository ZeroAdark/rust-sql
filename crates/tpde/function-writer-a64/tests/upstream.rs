// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![cfg(feature = "upstream-oracle")]

mod oracle_build;

use std::{
    any::Any,
    fs,
    panic::{catch_unwind, AssertUnwindSafe},
    path::Path,
    process::{Command, Output, Stdio},
};
use tpde_assembler::{Architecture, AssemblerElf, SectionKind, SymRef};
use tpde_core::Reg;
use tpde_function_writer::{FunctionWriterBase, Label, LabelFixupKind, ModuleWriter};
use tpde_function_writer_a64::{A64Target, A64WriteInst, FunctionWriterA64};

type Writer<'w, 'a, 'm> = ModuleWriter<'w, 'a, 'm, A64Target<'m>>;
const NOP: u32 = 0xd503201f;
const HERE: u64 = u64::MAX;
const CPP_MODES: usize = 3;
const ISLAND_DEFECT_CASE: &str = "twenty-one-veneer-islands-reverse-false";

#[derive(Clone)]
struct Op {
    code: u8,
    args: [u64; 4],
    labels: Vec<u32>,
}
fn op(code: u8, args: [u64; 4]) -> Op {
    Op {
        code,
        args,
        labels: vec![],
    }
}
fn table(index: u64, temporary: u64, width32: bool, labels: &[u32]) -> Op {
    Op {
        code: 9,
        args: [index, temporary, u64::from(width32), 0],
        labels: labels.into(),
    }
}
fn put(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn blob(out: &mut Vec<u8>, data: &[u8]) {
    put(out, data.len() as u64);
    out.extend_from_slice(data);
}
fn state(w: &FunctionWriterBase<'_, '_, '_>, out: &mut Vec<u8>) {
    for value in [
        w.offset() as u64,
        w.allocated_size() as u64,
        u64::from(w.func_begin()),
        u64::from(w.label_skew()),
        u64::from(w.get_sec_ref().id()),
        u64::from(w.get_section().unwrap().align),
    ] {
        put(out, value);
    }
}
fn tables(w: &FunctionWriterBase<'_, '_, '_>, out: &mut Vec<u8>) {
    put(out, w.jump_table_count() as u64);
    for index in 0..w.jump_table_count() {
        let h = w.jump_table_header(index).unwrap();
        for value in [
            u64::from(h.off),
            u64::from(h.size()),
            u64::from(h.idx.id()),
            u64::from(h.tmp.id()),
            u64::from(h.misc),
        ] {
            put(out, value);
        }
        for label in w.jump_table_labels(index).unwrap() {
            put(out, u64::from(label.0));
        }
    }
}
fn pad_to(w: &mut Writer<'_, '_, '_>, end: usize) {
    assert!(end.is_multiple_of(4) && end >= w.offset());
    while w.offset() < end {
        w.write_inst(NOP).unwrap();
    }
    assert_eq!(w.offset(), end);
}
fn branch(w: &mut Writer<'_, '_, '_>, label: u32, kind: u8, inst: u32) {
    w.ensure_space(4).unwrap();
    let off = w.offset() as u32;
    w.label_ref(Label(label), off, LabelFixupKind(kind))
        .unwrap();
    w.write_inst_unchecked(inst).unwrap();
}
fn word(w: &FunctionWriterBase<'_, '_, '_>, off: usize) -> u32 {
    u32::from_le_bytes(w.data().unwrap()[off..off + 4].try_into().unwrap())
}
fn signed(value: u32, bits: u32) -> i64 {
    i64::from((value << (32 - bits)) as i32 >> (32 - bits))
}
fn validate_branches(w: &FunctionWriterBase<'_, '_, '_>) -> (usize, usize) {
    let (mut direct, mut veneers) = (0, 0);
    for fixup in w.label_fixups() {
        let off = i64::from(fixup.off - w.label_skew());
        let inst = word(w, off as usize);
        let expected = i64::from(w.label_offset(fixup.label).unwrap());
        let bits = match fixup.kind {
            LabelFixupKind::AARCH64_BR => 26,
            LabelFixupKind::AARCH64_COND_BR => 19,
            LabelFixupKind::AARCH64_TEST_BR => 14,
            _ => panic!("unexpected fixup"),
        };
        let immediate = if bits == 26 {
            inst & 0x03ff_ffff
        } else {
            (inst >> 5) & ((1 << bits) - 1)
        };
        let target = off + signed(immediate, bits) * 4;
        if target == expected {
            direct += 1;
        } else {
            assert_ne!(bits, 26, "unconditional branch did not reach its label");
            let veneer = word(w, target as usize);
            assert_eq!(
                veneer & 0xfc00_0000,
                0x1400_0000,
                "veneer is not an unconditional branch"
            );
            assert_eq!(target + signed(veneer & 0x03ff_ffff, 26) * 4, expected);
            veneers += 1;
        }
    }
    (direct, veneers)
}
#[derive(Default)]
struct Run {
    current_step: Option<usize>,
    expected: Vec<u8>,
    records: Vec<(usize, usize, String)>,
    direct: usize,
    veneers: usize,
}
fn encode_ops(ops: &[Op]) -> Vec<u8> {
    let mut input = Vec::new();
    for operation in ops {
        input.push(operation.code);
        for &arg in &operation.args {
            put(&mut input, arg);
        }
        input.extend_from_slice(&(operation.labels.len() as u32).to_le_bytes());
        for &label in &operation.labels {
            input.extend_from_slice(&label.to_le_bytes());
        }
    }
    input
}
fn run(ops: &[Op], result: &mut Run) {
    let context = mcx::MemoryContext::new("A64 writer oracle");
    let mut assembler = AssemblerElf::new(context.mcx(), Architecture::Aarch64).unwrap();
    let sections = [
        assembler.get_default_section(SectionKind::Text).unwrap(),
        assembler.create_section(SectionKind::Text).unwrap(),
    ];
    let mut owner = FunctionWriterA64::new(context.mcx(), A64Target::new(context.mcx()));
    {
        let mut writer = owner.begin_module(&mut assembler).unwrap();
        writer.switch_section(sections[0]).unwrap();
        let mut begun = false;
        let mut original_branches = Vec::new();
        for (step, operation) in ops.iter().enumerate() {
            let Op {
                code,
                args: [x, y, z, _],
                labels,
            } = operation;
            result.current_step = Some(step);
            let start = result.expected.len();
            result.expected.push(*code);
            match *code {
                0 => {
                    writer.begin_func(*x as u32, *y as u32).unwrap();
                    begun = true;
                    original_branches.clear();
                }
                1 => writer.ensure_space(*x as usize).unwrap(),
                2 => writer.write_inst(*x as u32).unwrap(),
                3 => put(
                    &mut result.expected,
                    u64::from(writer.try_write_inst(*x as u32).unwrap()),
                ),
                4 => writer.write_inst_unchecked(*x as u32).unwrap(),
                5 => pad_to(&mut writer, *x as usize),
                6 => put(
                    &mut result.expected,
                    u64::from(writer.label_create().unwrap().0),
                ),
                7 => {
                    let off = if *y == HERE {
                        writer.offset() as u32
                    } else {
                        *y as u32
                    };
                    writer.label_place(Label(*x as u32), off).unwrap();
                }
                8 => {
                    branch(&mut writer, *x as u32, *y as u8, *z as u32);
                    original_branches.push((writer.offset() as u32 - 4, *z as u32));
                }
                9 => {
                    writer
                        .create_jump_table(
                            labels.len() as u32,
                            Reg::new(*x),
                            Reg::new(*y),
                            *z != 0,
                            |out| {
                                for (slot, &label) in out.iter_mut().zip(labels) {
                                    *slot = Label(label);
                                }
                            },
                        )
                        .unwrap();
                    tables(&writer, &mut result.expected);
                }
                10 => writer.remove_prologue_bytes(*x as u32, *y as u32).unwrap(),
                11 => writer.eh_begin_fde(SymRef::default()).unwrap(),
                12 => writer.eh_advance(*x).unwrap(),
                13 => writer.eh_end_fde().unwrap(),
                14 => {
                    writer.finish_func().unwrap();
                    assert_eq!(writer.label_fixups().len(), original_branches.len());
                    for (fixup, &(raw_off, original)) in
                        writer.label_fixups().iter().zip(&original_branches)
                    {
                        assert_eq!(fixup.off, raw_off);
                        let mask = match fixup.kind {
                            LabelFixupKind::AARCH64_BR => 0x03ff_ffff,
                            LabelFixupKind::AARCH64_COND_BR => ((1 << 19) - 1) << 5,
                            LabelFixupKind::AARCH64_TEST_BR => ((1 << 14) - 1) << 5,
                            _ => panic!("unexpected branch kind"),
                        };
                        assert_eq!(
                            word(&writer, (raw_off - writer.label_skew()) as usize) & !mask,
                            original & !mask,
                            "branch opcode, condition, or register changed at {raw_off}"
                        );
                    }
                    let (direct, veneers) = validate_branches(&writer);
                    result.direct += direct;
                    result.veneers += veneers;
                    tables(&writer, &mut result.expected);
                    blob(&mut result.expected, writer.data().unwrap());
                }
                15 => {
                    if begun {
                        writer.ensure_space(1).unwrap();
                    }
                    writer.flush().unwrap();
                    writer.end_module().unwrap();
                }
                16 => put(
                    &mut result.expected,
                    u64::from(writer.label_offset(Label(*x as u32)).unwrap()),
                ),
                17 => writer.align(*x as usize).unwrap(),
                18 => {
                    if begun {
                        writer.ensure_space(1).unwrap();
                    }
                    writer.flush().unwrap();
                    writer.switch_section(sections[*x as usize]).unwrap();
                }
                _ => panic!("unknown operation {code}"),
            }
            state(&writer, &mut result.expected);
            result.records.push((
                start,
                result.expected.len(),
                format!("step={step} opcode={code} args={:?}", operation.args),
            ));
        }
    }
    assembler.finalize();
    let start = result.expected.len();
    result.expected.push(255);
    blob(
        &mut result.expected,
        &assembler.build_object_file().unwrap(),
    );
    result
        .records
        .push((start, result.expected.len(), "complete ELF image".into()));
    result.current_step = None;
}
fn end(ops: &mut Vec<Op>) {
    ops.extend([op(14, [0; 4]), op(15, [0; 4])]);
}
fn cases() -> Vec<(String, Vec<Op>)> {
    let mut cases = vec![("empty-module".into(), vec![op(15, [0; 4])])];
    for hint in [0, 4, 32768, 1048576] {
        let mut ops = vec![
            op(0, [4, hint, 0, 0]),
            op(11, [0; 4]),
            op(3, [0; 4]),
            op(3, [u64::from(NOP), 0, 0, 0]),
            op(1, [64, 0, 0, 0]),
        ];
        for inst in [0xd65f03c0, 0xffffffff, 1, NOP] {
            ops.push(op(4, [u64::from(inst), 0, 0, 0]));
        }
        for size in [
            0,
            1,
            3,
            4,
            252,
            255,
            256,
            1020,
            1023,
            1024,
            262140,
            262143,
            262144,
            u64::from(u32::MAX) * 4 + 3,
        ] {
            ops.push(op(12, [size, 0, 0, 0]));
        }
        ops.extend([op(5, [100000, 0, 0, 0]), op(13, [0; 4])]);
        end(&mut ops);
        cases.push((format!("emission-cfi-hint-{hint}"), ops));
    }
    for align in [32768, 65536] {
        let mut ops = vec![
            op(0, [align, 0, 0, 0]),
            op(2, [u64::from(NOP), 0, 0, 0]),
            op(5, [align - 4, 0, 0, 0]),
            op(17, [align, 0, 0, 0]),
        ];
        end(&mut ops);
        cases.push((format!("large-align-{align}"), ops));
    }
    for (kind, inst) in [(0, 0x14000000u32), (1, 0xb5000003), (2, 0xb7000004)] {
        for distance in [0, 4, 32764, 32768, 32772, 1048572, 1048576, 1048580] {
            let mut ops = vec![
                op(0, [4, 65536, 0, 0]),
                op(6, [0; 4]),
                op(8, [0, kind, u64::from(inst), 0]),
            ];
            if distance >= 4 {
                ops.push(op(5, [distance, 0, 0, 0]));
            }
            ops.push(op(7, [0, distance, 0, 0]));
            end(&mut ops);
            cases.push((format!("branch-kind-{kind}-distance-{distance}"), ops));
        }
    }
    for skew in [0, 16] {
        let mut ops = vec![];
        for function in 0..3 {
            ops.push(op(18, [(function % 2) as u64, 0, 0, 0]));
            ops.push(op(0, [16, 65536, 0, 0]));
            ops.push(op(11, [0; 4]));
            let start = if function == 2 { 1114112 - skew } else { 0 };
            ops.push(op(5, [start + 32, 0, 0, 0]));
            for (index, kind, inst) in [
                (0, 1, 0x54000001),
                (1, 1, 0xb4000009),
                (2, 2, 0x3600000a),
                (3, 0, 0x14000000),
            ] {
                ops.push(op(6, [0; 4]));
                ops.push(op(8, [index, kind, inst, 0]));
            }
            ops.push(op(5, [start + 1114112, 0, 0, 0]));
            for label in 0..4 {
                ops.push(op(7, [label, HERE, 0, 0]));
            }
            ops.push(op(10, [start, skew, 0, 0]));
            ops.push(op(13, [0; 4]));
            ops.push(op(14, [0; 4]));
            if function == 0 {
                ops.push(op(17, [16, 0, 0, 0]));
            }
        }
        ops.push(op(15, [0; 4]));
        cases.push((format!("mixed-branches-sections-reuse-skew-{skew}"), ops));
    }
    for reverse in [false, true] {
        let mut ops = vec![op(0, [4, 65536, 0, 0])];
        for _ in 0..63 {
            ops.push(op(6, [0; 4]));
        }
        for island in 0..21 {
            let order = if reverse { [2, 1, 0] } else { [0, 1, 2] };
            for index in order {
                let label = island * 3 + index;
                ops.push(op(
                    8,
                    [
                        label,
                        if index == 2 { 1 } else { 2 },
                        if index == 2 {
                            0x54000000
                        } else {
                            0x36000000 + index
                        },
                        0,
                    ],
                ));
            }
            ops.push(op(5, [(island + 1) * 32768, 0, 0, 0]));
        }
        ops.push(op(5, [786432, 0, 0, 0]));
        for label in (0..63).rev() {
            ops.push(op(7, [label, HERE, 0, 0]));
        }
        ops.push(op(14, [0; 4]));
        ops.push(op(0, [16, 1048576, 0, 0]));
        ops.push(op(6, [0; 4]));
        ops.push(op(8, [0, 2, 0x36000007, 0]));
        ops.push(op(5, [851968, 0, 0, 0]));
        ops.push(op(7, [0, HERE, 0, 0]));
        end(&mut ops);
        cases.push((format!("twenty-one-veneer-islands-reverse-{reverse}"), ops));
    }
    for width32 in [false, true] {
        for span in [0, 1020, 1024, 262140, 262144, 262148] {
            let mut ops = vec![
                op(0, [4, 65536, 0, 0]),
                op(6, [0; 4]),
                op(6, [0; 4]),
                op(1, [24, 0, 0, 0]),
                table(7, 19, width32, &[1, 0, 1, 0]),
            ];
            let end_offset = (12 + span).max(24);
            ops.push(op(5, [end_offset, 0, 0, 0]));
            ops.extend([
                op(7, [0, end_offset, 0, 0]),
                op(7, [1, end_offset - if span == 0 { 0 } else { 4 }, 0, 0]),
            ]);
            end(&mut ops);
            cases.push((format!("table-span-{span}-width32-{width32}"), ops));
        }
        for (index, temporary) in [(0, 1), (7, 19), (29, 30), (16, 17)] {
            let mut ops = vec![
                op(0, [4, 65536, 0, 0]),
                op(6, [0; 4]),
                op(7, [0, 0, 0, 0]),
                op(5, [128, 0, 0, 0]),
                op(1, [24, 0, 0, 0]),
                table(index, temporary, width32, &[0, 0, 0]),
                op(5, [256, 0, 0, 0]),
            ];
            end(&mut ops);
            cases.push((
                format!("small-backward-table-r{index}-r{temporary}-width32-{width32}"),
                ops,
            ));
        }
    }
    let mut ops = vec![op(0, [4, 65536, 0, 0])];
    for _ in 0..81 {
        ops.push(op(6, [0; 4]));
    }
    for index in 0..35 {
        ops.push(op(1, [24, 0, 0, 0]));
        let labels = if index == 0 {
            vec![]
        } else {
            (0..128).map(|i| (i * 7 + index) % 81).collect::<Vec<_>>()
        };
        ops.push(table((index % 15) as u64, 28, index % 2 == 0, &labels));
    }
    ops.push(op(5, [4096, 0, 0, 0]));
    for label in 0..81 {
        ops.push(op(7, [label, HERE, 0, 0]));
    }
    end(&mut ops);
    cases.push(("table-and-label-storage-spills".into(), ops));
    cases
}
struct Failures {
    entries: Vec<String>,
}
impl Failures {
    fn write(&mut self, path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) {
        if let Err(error) = fs::write(path.as_ref(), bytes) {
            self.entries
                .push(format!("artifact {}: {error}", path.as_ref().display()));
        }
    }
    fn directory(&mut self, path: &Path) {
        if let Err(error) = fs::create_dir_all(path) {
            self.entries
                .push(format!("directory {}: {error}", path.display()));
        }
    }
    fn capture<T>(&mut self, dir: &Path, stem: &str, action: impl FnOnce() -> T) -> Option<T> {
        match catch_unwind(AssertUnwindSafe(action)) {
            Ok(value) => {
                self.write(dir.join(format!("{stem}-verdict.txt")), "PASS\n");
                Some(value)
            }
            Err(payload) => {
                let message = panic_message(payload);
                self.write(dir.join(format!("{stem}-verdict.txt")), "FAIL\n");
                self.write(dir.join(format!("{stem}-error.txt")), &message);
                if stem == "rust" {
                    self.write(dir.join("rust-stderr.txt"), &message);
                }
                self.entries
                    .push(format!("{} {stem}: {message}", dir.display()));
                None
            }
        }
    }
    fn finish(mut self, work: &Path) {
        let report = self.entries.join("\n\n");
        self.write(work.join("failures.txt"), &report);
        assert!(
            self.entries.is_empty(),
            "A64 oracle completed its roster with {} failures:\n{}",
            self.entries.len(),
            self.entries.join("\n\n")
        );
    }
}
fn panic_message(payload: Box<dyn Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).into()
    } else {
        "non-string panic payload".into()
    }
}
fn checked_output(command: &mut Command, dir: &Path, stem: &str) -> Output {
    fs::write(
        dir.join(format!("{stem}-command.txt")),
        format!("{command:?}\n"),
    )
    .unwrap();
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("{stem}: {error}"));
    fs::write(dir.join(format!("{stem}-stdout.bin")), &output.stdout).unwrap();
    fs::write(dir.join(format!("{stem}-stderr.txt")), &output.stderr).unwrap();
    fs::write(
        dir.join(format!("{stem}-exit-status.txt")),
        format!("{}\n", output.status),
    )
    .unwrap();
    output
}
fn compare(
    reference: &oracle_build::Reference,
    dir: &Path,
    input: &Path,
    run: Option<&Run>,
    name: &str,
) -> &'static str {
    let output = checked_output(
        Command::new(&reference.executable).stdin(Stdio::from(fs::File::open(input).unwrap())),
        dir,
        reference.name,
    );
    if name == ISLAND_DEFECT_CASE {
        let Some(run) = run else {
            return "CPP_EXECUTED_RUST_FAILED";
        };
        compare_island_defect(reference, &output, run);
        return "EXPECTED_UPSTREAM_DEFECT_RUST_CORRECT";
    }
    assert!(
        output.status.success(),
        "{name}, {}: C++ failed: {}\n{}",
        reference.name,
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let Some(run) = run else {
        return "CPP_EXECUTED_RUST_FAILED";
    };
    if run.expected != output.stdout {
        let byte = run
            .expected
            .iter()
            .zip(&output.stdout)
            .position(|(a, b)| a != b)
            .unwrap_or(run.expected.len().min(output.stdout.len()));
        let record = run
            .records
            .iter()
            .find(|(start, end, _)| *start <= byte && byte < *end);
        let start = byte.saturating_sub(16);
        let end = (byte + 24).min(run.expected.len()).min(output.stdout.len());
        panic!("{name}, {}: mismatch at byte {byte}, record {record:?}, Rust length {}, C++ length {}; Rust {:02x?}, C++ {:02x?}", reference.name, run.expected.len(), output.stdout.len(), &run.expected[start..end], &output.stdout[start..end]);
    }
    "MATCH"
}
fn read_u64(bytes: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap())
}
fn patch_island_defect(text: &mut [u8]) {
    for (off, rust, cpp) in [
        (65548, 0x36020021u32, 0x1400001du32),
        (81936, 0x1402affc, 0),
        (163852, 0x36020021, 0x1400001d),
        (180240, 0x14024ffc, 0),
    ] {
        assert_eq!(
            u32::from_le_bytes(text[off..off + 4].try_into().unwrap()),
            rust,
            "corrected Rust instruction at {off}"
        );
        text[off..off + 4].copy_from_slice(&cpp.to_le_bytes());
    }
}
fn compare_island_defect(reference: &oracle_build::Reference, output: &Output, run: &Run) {
    assert_eq!((run.direct, run.veneers), (21, 43));
    let text_records = run
        .records
        .iter()
        .filter(|&&(start, _, _)| run.expected[start] == 14)
        .collect::<Vec<_>>();
    assert_eq!(text_records.len(), 2);
    if reference.assertions {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(output.status.signal(), Some(6), "expected upstream SIGABRT");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("FunctionWriterA64.cpp:88:")
                && stderr.contains("overwriting instructions with veneer branch"),
            "unexpected upstream assertion: {stderr}"
        );
        assert_eq!(output.stdout, run.expected[..text_records[0].0]);
        return;
    }
    assert!(
        output.status.success(),
        "unexpected upstream exit: {}",
        output.status
    );
    assert!(output.stderr.is_empty());
    let mut expected = run.expected.clone();
    for (&(start, end, _), size) in text_records.into_iter().zip([786432usize, 851968]) {
        assert_eq!(read_u64(&expected, start + 1), 0);
        assert_eq!(read_u64(&expected, start + 9) as usize, size);
        assert_eq!(end, start + 17 + size + 48);
        patch_island_defect(&mut expected[start + 17..start + 17 + size]);
    }
    let (start, end, _) = run.records.last().unwrap();
    assert_eq!(expected[*start], 255);
    assert_eq!(read_u64(&expected, start + 1) as usize, end - start - 9);
    let elf = &mut expected[start + 9..*end];
    assert_eq!(&elf[..6], b"\x7fELF\x02\x01");
    let shoff = read_u64(elf, 40) as usize;
    let shentsize = u16::from_le_bytes(elf[58..60].try_into().unwrap()) as usize;
    let shnum = u16::from_le_bytes(elf[60..62].try_into().unwrap()) as usize;
    assert_eq!(shentsize, 64);
    let executable = (0..shnum)
        .filter_map(|index| {
            let header = shoff + index * shentsize;
            let kind = u32::from_le_bytes(elf[header + 4..header + 8].try_into().unwrap());
            let flags = read_u64(elf, header + 8);
            let size = read_u64(elf, header + 32) as usize;
            (kind == 1 && flags & 4 != 0 && size != 0)
                .then(|| (read_u64(elf, header + 24) as usize, size))
        })
        .collect::<Vec<_>>();
    assert_eq!(executable.len(), 1);
    let (offset, size) = executable[0];
    assert_eq!(size, 851968);
    patch_island_defect(&mut elf[offset..offset + size]);
    if output.stdout != expected {
        let byte = output
            .stdout
            .iter()
            .zip(&expected)
            .position(|(actual, expected)| actual != expected)
            .unwrap_or(output.stdout.len().min(expected.len()));
        panic!(
            "upstream differs outside the four independently specified defective words at byte {byte}; expected length {}, actual length {}",
            expected.len(),
            output.stdout.len()
        );
    }
}
fn numbers(output: &Output, name: &str) -> Vec<u64> {
    assert!(
        output.status.success(),
        "{name}: {}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.len().is_multiple_of(8));
    output
        .stdout
        .chunks_exact(8)
        .map(|bytes| u64::from_le_bytes(bytes.try_into().unwrap()))
        .collect()
}
fn diagnostic_rust(name: &str) -> Vec<u64> {
    let context = mcx::MemoryContext::new("A64 writer upstream defect control");
    let mut assembler = AssemblerElf::new(context.mcx(), Architecture::Aarch64).unwrap();
    let section = assembler.get_default_section(SectionKind::Text).unwrap();
    let mut owner = FunctionWriterA64::new(context.mcx(), A64Target::new(context.mcx()));
    let mut writer = owner.begin_module(&mut assembler).unwrap();
    writer.switch_section(section).unwrap();
    if name == "--capped-flush" {
        writer.begin_func(4, 65536).unwrap();
        let label = writer.label_create().unwrap();
        branch(&mut writer, label.0, 0, 0x14000000);
        pad_to(&mut writer, 32768);
        writer.label_place(label, 32768).unwrap();
        writer.finish_func().unwrap();
        assert_eq!(validate_branches(&writer), (1, 0));
        writer.flush().unwrap();
        assert_eq!(writer.get_section().unwrap().data.len(), 32768);
        return vec![
            writer.offset() as u64,
            writer.allocated_size() as u64,
            writer.get_section().unwrap().data.len() as u64,
            u64::from(word(&writer, 0)),
        ];
    }
    if name == "--table-growth" {
        writer.begin_func(4, 32768).unwrap();
        let label = writer.label_create().unwrap();
        branch(&mut writer, label.0, 2, 0x36000000);
        pad_to(&mut writer, 16372);
        writer
            .create_jump_table(1, Reg::new(1), Reg::new(2), false, |labels| {
                labels[0] = label
            })
            .unwrap();
        let off = writer.jump_table_header(0).unwrap().off;
        let cursor = writer.offset();
        let before = word(&writer, 16372);
        assert_eq!((off, cursor, before), (16380, 16404, 0x14000002));
        pad_to(&mut writer, 65536);
        writer.label_place(label, 65536).unwrap();
        writer.finish_func().unwrap();
        assert_eq!(validate_branches(&writer), (0, 1));
        assert_eq!(word(&writer, 16372), 0x14000002);
        assert_eq!(word(&writer, 16376) & 0xfc000000, 0x14000000);
        assert_eq!(word(&writer, off as usize) & 0x9f000000, 0x90000000);
        return vec![
            u64::from(off),
            cursor as u64,
            u64::from(before),
            u64::from(word(&writer, 16372)),
            u64::from(word(&writer, 16376)),
            u64::from(word(&writer, 0)),
        ];
    }
    if name == "--wide-backward" {
        writer.begin_func(4, 65536).unwrap();
        pad_to(&mut writer, 16);
        let label = writer.label_create().unwrap();
        writer.label_place(label, 16).unwrap();
        pad_to(&mut writer, 300000);
        writer.ensure_space(24).unwrap();
        writer
            .create_jump_table(1, Reg::new(1), Reg::new(2), false, |labels| {
                labels[0] = label
            })
            .unwrap();
        writer.finish_func().unwrap();
        let off = writer.jump_table_header(0).unwrap().off;
        let section = writer.get_jump_table_section().unwrap();
        let bytes = &writer.assembler().get_section(section).unwrap().data;
        assert_eq!(bytes.len(), 4);
        let entry = u32::from_le_bytes(bytes[..4].try_into().unwrap());
        let destination = i64::from(off + 12) + i64::from(entry as i32) * 4;
        assert_eq!(destination, 16);
        return vec![
            u64::from(off),
            bytes.len() as u64,
            u64::from(entry),
            destination as u64,
        ];
    }
    if name.starts_with("--veneer-") {
        let skew = if name.contains("s16") { 16 } else { 0 };
        let gap = if name.ends_with("g8") {
            8
        } else if name.ends_with("g4") {
            4
        } else {
            0
        };
        writer.begin_func(4, 65536).unwrap();
        pad_to(&mut writer, skew);
        let label = writer.label_create().unwrap();
        let first = writer.offset();
        branch(&mut writer, label.0, 2, 0x36000000);
        pad_to(&mut writer, 16384);
        writer.ensure_space(4).unwrap();
        assert_eq!(writer.offset(), 16392);
        pad_to(&mut writer, 16392 + gap);
        let second = writer.offset();
        branch(&mut writer, label.0, 2, 0x36000001);
        pad_to(&mut writer, 65536);
        writer.label_place(label, 65536).unwrap();
        writer.remove_prologue_bytes(0, skew as u32).unwrap();
        writer.finish_func().unwrap();
        assert_eq!(validate_branches(&writer), (0, 2));
        let first_word = word(&writer, first - skew);
        let second_word = word(&writer, second - skew);
        assert_eq!(first_word & !(((1 << 14) - 1) << 5), 0x36000000);
        assert_eq!(second_word & !(((1 << 14) - 1) << 5), 0x36000001);
        return vec![
            first as u64,
            second as u64,
            writer.offset() as u64,
            u64::from(first_word),
            u64::from(second_word),
        ];
    }
    panic!("unknown diagnostic {name}");
}
fn limit_rust() {
    let context = mcx::MemoryContext::new("A64 writer section limit");
    let mut assembler = AssemblerElf::new(context.mcx(), Architecture::Aarch64).unwrap();
    let section = assembler.get_default_section(SectionKind::Text).unwrap();
    let mut owner = FunctionWriterA64::new(context.mcx(), A64Target::new(context.mcx()));
    let mut writer = owner.begin_module(&mut assembler).unwrap();
    writer.switch_section(section).unwrap();
    let limit = 128 * 1024 * 1024;
    writer.begin_func(4, limit).unwrap();
    while writer.offset() < limit as usize {
        writer.skip(16384).unwrap();
    }
    let error = writer.ensure_space(4).unwrap_err();
    assert!(error
        .to_string()
        .contains("AArch64 doesn't support sections larger than 128 MiB"));
    assert_eq!(writer.offset(), limit as usize);
    assert_eq!(writer.allocated_size(), limit as usize);
}
fn diagnostics(
    references: &[oracle_build::Reference],
    work: &Path,
    failures: &mut Failures,
) -> usize {
    let mut count = 0;
    let mut verdicts = String::from("case\tmode\tverdict\n");
    let mut rust_verdicts = String::from("case\tverdict\n");
    for name in [
        "--capped-flush",
        "--table-growth",
        "--wide-backward",
        "--veneer-s0-g0",
        "--veneer-s16-g0",
        "--veneer-s16-g4",
        "--veneer-s16-g8",
        "--limit",
    ] {
        let dir = work.join(name.trim_start_matches('-'));
        failures.directory(&dir);
        failures.write(dir.join("input.txt"), format!("{name}\n"));
        failures.write(dir.join("rust-stderr.txt"), "");
        let rust = failures.capture(&dir, "rust", || {
            if name == "--limit" {
                limit_rust();
                Vec::new()
            } else {
                diagnostic_rust(name)
            }
        });
        if let Some(observations) = &rust {
            failures.write(
                dir.join("rust-observations.txt"),
                format!("{observations:?}\n"),
            );
        }
        rust_verdicts.push_str(&format!(
            "{name}\t{}\n",
            if rust.is_some() { "PASS" } else { "FAIL" }
        ));
        failures.write(work.join("rust-diagnostic-verdicts.tsv"), &rust_verdicts);
        for reference in references {
            let result = failures.capture(&dir, reference.name, || {
                let output = checked_output(
                    Command::new(&reference.executable).arg(name),
                    &dir,
                    reference.name,
                );
                if name == "--limit" {
                    assert!(
                        output.status.success(),
                        "limit refusal failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                    assert_eq!(
                        output.stdout,
                        b"AArch64 doesn't support sections larger than 128 MiB\n"
                    );
                } else if name.starts_with("--veneer-") && reference.assertions {
                    assert!(
                        !output.status.success(),
                        "{name}: expected the original C++ exhausted-veneer assertion"
                    );
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    assert!(
                        stderr.contains("overwriting instructions with veneer branch"),
                        "{name}: wrong refusal: {stderr}"
                    );
                } else {
                    let cpp = numbers(&output, name);
                    match name {
                        "--capped-flush" => {
                            assert_eq!(cpp, vec![32768, 32768, 65540, 0x14002000]);
                        }
                        "--table-growth" => {
                            assert_eq!(cpp.len(), 6);
                            assert_eq!(&cpp[..3], &[16372, 16404, 0x14000002]);
                            assert_eq!(cpp[3] as u32 & 0x9f000000, 0x90000000);
                            assert_eq!(cpp[4] as u32 & 0xffc00000, 0x91000000);
                            assert_eq!(cpp[5], 0x3601ffc0);
                        }
                        "--wide-backward" => {
                            assert_eq!(
                                cpp,
                                vec![
                                    300000,
                                    4,
                                    u64::from(16u32.wrapping_sub(300012) >> 2),
                                    (1u64 << 32) + 16
                                ]
                            );
                        }
                        _ => {
                            let skew = if name.contains("s16") { 16 } else { 0 };
                            let gap = if name.ends_with("g8") {
                                8
                            } else if name.ends_with("g4") {
                                4
                            } else {
                                0
                            };
                            assert_eq!(cpp.len(), 5);
                            assert_eq!(&cpp[..3], &[skew, 16392 + gap, 65536 - skew]);
                            assert_ne!(cpp[4] as u32 & !(((1 << 14) - 1) << 5), 0x36000001);
                        }
                    }
                    if let Some(rust) = &rust {
                        match name {
                            "--table-growth" => assert_eq!(cpp[5], rust[5]),
                            "--wide-backward" => assert_eq!(rust[3], 16),
                            name if name.starts_with("--veneer-") => {
                                assert_eq!(&cpp[..3], &rust[..3]);
                            }
                            _ => {}
                        }
                        assert_ne!(
                            &cpp, rust,
                            "{name}: the source defect must remain independently visible"
                        );
                    }
                }
            });
            count += 1;
            let verdict = if result.is_none() {
                "FAIL"
            } else if rust.is_none() {
                "CPP_CHECKED_RUST_FAILED"
            } else if name == "--limit" {
                "MATCH"
            } else {
                "EXPECTED_UPSTREAM_DEFECT_RUST_CORRECT"
            };
            failures.write(
                dir.join(format!("{}-verdict.txt", reference.name)),
                format!("{verdict}\n"),
            );
            verdicts.push_str(&format!("{name}\t{}\t{verdict}\n", reference.name));
            failures.write(work.join("diagnostic-verdicts.tsv"), &verdicts);
        }
    }
    count
}
#[test]
fn complete_a64_writer_matches_pinned_cpp_and_preserves_branch_destinations() {
    let work = oracle_build::prepare();
    let references = oracle_build::require_references(&work);
    let cases = cases();
    let mut failures = Failures { entries: vec![] };
    let mut roster =
        String::from("case\tmode\toperations\tdirect_branches\tveneer_branches\tverdict\n");
    let mut rust_roster = String::from("case\toperations\tverdict\n");
    let (mut operations, mut direct, mut veneers, mut comparisons) = (0, 0, 0, 0);
    let (mut matching_comparisons, mut scripted_defect_observations) = (0, 0);
    for (index, (name, ops)) in cases.iter().enumerate() {
        let dir = work.join(format!("case-{index:03}-{name}"));
        failures.directory(&dir);
        let input = dir.join("input.bin");
        failures.write(&input, encode_ops(ops));
        failures.write(
            dir.join("operations.txt"),
            ops.iter()
                .enumerate()
                .map(|(step, operation)| {
                    format!(
                        "step={step} opcode={} args={:?} labels={:?}\n",
                        operation.code, operation.args, operation.labels
                    )
                })
                .collect::<String>(),
        );
        failures.write(dir.join("rust-stderr.txt"), "");
        let mut result = Run::default();
        let rust_ok = failures
            .capture(&dir, "rust", || run(ops, &mut result))
            .is_some();
        failures.write(dir.join("rust-output.bin"), &result.expected);
        failures.write(
            dir.join("records.txt"),
            format!(
                "current_step={:?}\n{:?}\n",
                result.current_step, result.records
            ),
        );
        operations += ops.len();
        if rust_ok {
            direct += result.direct;
            veneers += result.veneers;
        }
        rust_roster.push_str(&format!(
            "{name}\t{}\t{}\n",
            ops.len(),
            if rust_ok { "PASS" } else { "FAIL" }
        ));
        failures.write(work.join("rust-case-verdicts.tsv"), &rust_roster);
        for reference in &references {
            let comparison = failures.capture(&dir, reference.name, || {
                compare(reference, &dir, &input, rust_ok.then_some(&result), name)
            });
            comparisons += 1;
            let verdict = match comparison {
                Some("MATCH") => {
                    matching_comparisons += 1;
                    "MATCH"
                }
                Some("EXPECTED_UPSTREAM_DEFECT_RUST_CORRECT") => {
                    scripted_defect_observations += 1;
                    "EXPECTED_UPSTREAM_DEFECT_RUST_CORRECT"
                }
                Some(verdict) => verdict,
                None => "FAIL",
            };
            failures.write(
                dir.join(format!("{}-verdict.txt", reference.name)),
                format!("{verdict}\n"),
            );
            roster.push_str(&format!(
                "{name}\t{}\t{}\t{}\t{}\t{verdict}\n",
                reference.name,
                ops.len(),
                result.direct,
                result.veneers
            ));
            failures.write(work.join("case-verdicts.tsv"), &roster);
        }
    }
    let diagnostic_comparisons = diagnostics(&references, &work, &mut failures);
    let _ = failures.capture(&work, "coverage", || {
        assert_eq!(references.len(), CPP_MODES);
        assert_eq!(cases.len(), 56);
        assert!(direct > 0 && veneers > 0);
        assert_eq!(comparisons, 56 * CPP_MODES);
        assert_eq!(diagnostic_comparisons, 8 * CPP_MODES);
        assert_eq!(matching_comparisons, 55 * CPP_MODES);
        assert_eq!(scripted_defect_observations, CPP_MODES);
    });
    failures.finish(&work);
    println!("TPDE_FUNCTION_WRITER_A64_ORACLE PASS scripts=56 cpp_modes=3 diagnostics=8 active={} comparisons={} diagnostic_comparisons={} matching_comparisons={} scripted_defect_observations={} operations={} direct_branches={} veneer_branches={} profile=all", usize::from(cfg!(tpde_address_sanitizer)), comparisons, diagnostic_comparisons, matching_comparisons, scripted_defect_observations, operations, direct, veneers);
}

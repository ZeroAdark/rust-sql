// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![cfg(feature = "upstream-oracle")]

mod reference_support;
use reference_support::checked;

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use tpde_assembler::{Architecture, AssemblerElf, SectionKind, SymBinding, SymRef};
use tpde_core::Reg;
use tpde_function_writer::{FunctionWriter, FunctionWriterBase, Label, LabelFixupKind};
use tpde_function_writer_x64::X64Target;

struct Op {
    code: u8,
    args: [u64; 4],
    data: Vec<u8>,
}
struct Script {
    name: &'static str,
    ops: Vec<Op>,
    offset: u32,
    begin: u32,
    labels: u32,
    tables: Vec<u32>,
    functions: usize,
}
impl Script {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            ops: vec![],
            offset: 0,
            begin: 0,
            labels: 0,
            tables: vec![],
            functions: 0,
        }
    }
    fn op(&mut self, code: u8, args: [u64; 4]) {
        self.ops.push(Op {
            code,
            args,
            data: vec![],
        });
    }
    fn begin(&mut self, align: u32, expected: u32) {
        self.op(1, [align as u64, expected as u64, 0, 0]);
        self.offset = (self.offset + align - 1) & !(align - 1);
        self.begin = self.offset;
        self.labels = 0;
        self.tables.clear();
        self.functions += 1;
    }
    fn bytes(&mut self, len: u32, seed: u8) {
        self.ops.push(Op {
            code: 2,
            args: [0; 4],
            data: (0..len)
                .map(|i| (i as u8).wrapping_mul(37).wrapping_add(seed))
                .collect(),
        });
        self.offset += len;
    }
    fn align(&mut self, align: u32) {
        self.op(3, [align as u64, 0, 0, 0]);
        self.offset = (self.offset + align - 1) & !(align - 1);
    }
    fn label(&mut self, offset: u32, pending_reference: Option<u32>) -> u32 {
        let label = self.labels;
        self.labels += 1;
        self.op(4, [0; 4]);
        if let Some(reference) = pending_reference {
            self.reference(label, reference);
        }
        self.op(5, [label as u64, offset as u64, 0, 0]);
        label
    }
    fn reference(&mut self, label: u32, offset: u32) {
        self.op(6, [label as u64, offset as u64, 0, 0]);
    }
    fn table(&mut self, labels: &[u32], index: u32, temporary: u32, width32: bool) {
        self.ops.push(Op {
            code: 8,
            args: [
                labels.len() as u64,
                index as u64,
                temporary as u64,
                width32 as u64,
            ],
            data: labels
                .iter()
                .flat_map(|label| label.to_le_bytes())
                .collect(),
        });
        self.offset += 18;
        self.tables.push(labels.len() as u32);
    }
    fn skew(&mut self, size: u32) {
        self.op(9, [self.begin as u64, size as u64, 0, 0]);
        self.offset -= size;
    }
    fn finish(&mut self) {
        self.op(7, [0; 4]);
        for &size in &self.tables {
            self.offset = (self.offset + 3) & !3;
            self.offset += size * 4;
        }
    }
    fn fde(&mut self, personality: u64) {
        self.op(10, [personality, 0, 0, 0]);
        self.op(13, [0x0c, 6, 16, 0]);
    }
    fn end_fde(&mut self) {
        self.op(12, [(self.offset - self.begin) as u64, 0, 0, 0]);
        self.op(11, [0; 4]);
    }
    fn symbol(&mut self) {
        self.ops.push(Op {
            code: 17,
            args: [self.begin as u64, (self.offset - self.begin) as u64, 0, 0],
            data: format!("f.{}", self.functions).into_bytes(),
        });
    }
}
fn scripts() -> Vec<Script> {
    let mut scripts = vec![Script::new("empty-cie")];
    let mut script = Script::new("alignment-residues");
    for align in [1, 2, 4, 8, 16, 32, 64, 128, 256] {
        for residue in 0..align {
            script.begin(align, residue % 3);
            script.bytes(align + residue, (align + residue) as u8);
            script.align(align);
            script.align(align);
            script.finish();
        }
    }
    script.begin(4096, 0);
    script.bytes(1, 0xa5);
    script.align(4096);
    script.finish();
    scripts.push(script);

    let mut script = Script::new("all-legal-register-pairs");
    for index in 0..16 {
        if index == 4 {
            continue;
        }
        for temporary in 0..16 {
            for width32 in [false, true] {
                script.begin(16, 24);
                script.fde(0);
                script.bytes(25, (index * 16 + temporary) as u8);
                let begin = script.begin;
                let labels: Vec<_> = (0..4).map(|i| script.label(begin + 12 + i, None)).collect();
                script.reference(labels[3], begin + 8);
                script.table(
                    &[
                        labels[3], labels[1], labels[2], labels[0], labels[3], labels[2],
                    ],
                    index,
                    temporary,
                    width32,
                );
                script.bytes((index + temporary) % 4, 0x5a);
                script.finish();
                script.end_fde();
                script.symbol();
            }
        }
    }
    scripts.push(script);

    let mut script = Script::new("rel32-wrapping");
    for (offset_index, offset) in [0, 1, 3, 4, 7, 63, 252, 255].into_iter().enumerate() {
        for delta in [
            0u32,
            1,
            3,
            4,
            127,
            128,
            0x7fff_fffe,
            0x7fff_ffff,
            0x8000_0000,
            0x8000_0004,
            0xffff_fffc,
            0xffff_fffd,
            0xffff_fffe,
            0xffff_ffff,
        ] {
            script.begin(1, 0);
            script.bytes(512, 0x39);
            let fixup = script.begin + offset;
            let target = fixup.wrapping_add(4).wrapping_add(delta);
            assert_ne!(target, u32::MAX);
            let label = script.label(target, (offset_index % 2 == 0).then_some(fixup));
            if offset_index % 2 != 0 {
                script.reference(label, fixup);
            }
            script.finish();
        }
    }
    scripts.push(script);

    let mut script = Script::new("table-size-padding-prologue");
    for size in [0u32, 1, 2, 3, 31, 32, 33, 127, 128, 129, 1025, 4097] {
        for padding in 0..4 {
            for skew in [0, 1, 7] {
                script.begin(8, 1);
                script.fde(0);
                script.bytes(48, 0xbc);
                let begin = script.begin;
                let labels: Vec<_> = (0..16)
                    .map(|i| script.label(begin + 16 + i, None))
                    .collect();
                script.reference(labels[15], begin + 8);
                script.op(14, [1, 2, (begin + 32) as u64, u64::MAX - 3]);
                let entries: Vec<_> = (0..size).map(|i| labels[15 - (i as usize % 16)]).collect();
                script.table(&entries, 12, 13, size % 2 != 0);
                script.bytes(padding, 0x83);
                script.skew(skew);
                script.finish();
                script.end_fde();
                script.symbol();
            }
        }
    }
    scripts.push(script);

    let mut script = Script::new("multiple-tables-and-function-reset");
    for round in 0..4u32 {
        script.begin(1 << round, round);
        script.fde(if round % 2 == 0 { 0 } else { 2 });
        script.bytes(40, 0x67);
        let begin = script.begin;
        let labels: Vec<_> = (0..8).map(|i| script.label(begin + 16 + i, None)).collect();
        for table in 0..65u32 {
            let entries: Vec<_> = (0..table % 34)
                .map(|i| labels[7 - (i as usize % 8)])
                .collect();
            script.table(
                &entries,
                if table % 2 == 0 { 12 } else { 13 },
                table % 16,
                table % 2 != 0,
            );
            script.bytes(table % 4, table as u8);
        }
        script.skew(round);
        script.finish();
        script.end_fde();
        script.symbol();
        script.begin(1, 0);
        script.bytes(1, 0xc3);
        script.finish();
        script.symbol();
    }
    scripts.push(script);

    let mut script = Script::new("section-switch-and-table-reset");
    script.begin(1, 0);
    script.bytes(32, 0xb5);
    let target = script.label(16, None);
    script.table(&[target; 9], 1, 13, false);
    script.finish();
    script.symbol();
    script.op(15, [0; 4]);
    let first_offset = script.offset;
    script.op(16, [1, 0, 0, 0]);
    script.offset = 0;
    script.begin(32, 0);
    script.fde(0);
    script.bytes(37, 0x99);
    let target = script.label(16, None);
    script.table(&[target; 3], 12, 5, true);
    script.finish();
    script.end_fde();
    script.symbol();
    script.op(15, [0; 4]);
    script.op(16, [0; 4]);
    script.offset = first_offset;
    script.begin(64, 17);
    script.bytes(19, 0xa7);
    script.finish();
    script.symbol();
    scripts.push(script);
    scripts
}

fn put(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_le_bytes());
}
fn blob(output: &mut Vec<u8>, data: &[u8]) {
    put(output, data.len() as u64);
    output.extend_from_slice(data);
}
fn state(writer: &FunctionWriterBase<'_, '_, '_>, output: &mut Vec<u8>) {
    for value in [
        writer.offset() as u64,
        writer.func_begin() as u64,
        writer.label_skew() as u64,
        writer.get_sec_ref().id() as u64,
        writer.get_section().unwrap().align as u64,
        writer.label_count() as u64,
        writer.label_fixups().len() as u64,
        writer.jump_table_count() as u64,
    ] {
        put(output, value);
    }
    for index in 0..writer.label_count() {
        let label = Label(index as u32);
        put(
            output,
            if writer.label_is_pending(label).unwrap() {
                u32::MAX
            } else {
                writer.label_offset(label).unwrap()
            } as u64,
        );
    }
}
fn run(script: &Script) -> (Vec<u8>, Vec<u8>, String) {
    let context = mcx::MemoryContext::new("x64 writer oracle");
    let mut assembler = AssemblerElf::new(context.mcx(), Architecture::X86_64).unwrap();
    let syms = [
        SymRef::default(),
        assembler
            .sym_add_undef(b"external", SymBinding::Global)
            .unwrap(),
        assembler
            .sym_add_undef(b"personality", SymBinding::Global)
            .unwrap(),
    ];
    let sections = [
        assembler.get_default_section(SectionKind::Text).unwrap(),
        assembler.create_section(SectionKind::Text).unwrap(),
    ];
    let mut owner = FunctionWriter::new(context.mcx(), X64Target::new());
    let mut input = vec![];
    let mut expected = vec![];
    let mut records = String::from("operation\topcode\tstart\tend\targs\n");
    {
        let mut writer = owner.begin_module(&mut assembler).unwrap();
        writer.switch_section(sections[0]).unwrap();
        for (index, op) in script.ops.iter().enumerate() {
            let [a, b, c, d] = op.args;
            input.push(op.code);
            for value in op.args {
                put(&mut input, value);
            }
            input.extend_from_slice(&(op.data.len() as u32).to_le_bytes());
            input.extend_from_slice(&op.data);
            let start = expected.len();
            expected.push(op.code);
            match op.code {
                1 => writer.begin_func(a as u32, b as u32).unwrap(),
                2 => writer.write_bytes(&op.data).unwrap(),
                3 => writer.align(a as usize).unwrap(),
                4 => put(&mut expected, writer.label_create().unwrap().0 as u64),
                5 => writer.label_place(Label(a as u32), b as u32).unwrap(),
                6 => writer
                    .label_ref(
                        Label(a as u32),
                        b as u32,
                        LabelFixupKind::X64_JMP_OR_MEM_DISP,
                    )
                    .unwrap(),
                7 => writer.finish_func().unwrap(),
                8 => {
                    let table = writer.jump_table_count();
                    writer
                        .create_jump_table(a as u32, Reg::new(b), Reg::new(c), d != 0, |labels| {
                            for (label, bytes) in labels.iter_mut().zip(op.data.chunks_exact(4)) {
                                *label = Label(u32::from_le_bytes(bytes.try_into().unwrap()));
                            }
                        })
                        .unwrap();
                    let header = writer.jump_table_header(table).unwrap();
                    for value in [
                        header.size() as u64,
                        header.off as u64,
                        header.idx.id() as u64,
                        header.tmp.id() as u64,
                    ] {
                        put(&mut expected, value);
                    }
                    for label in writer.jump_table_labels(table).unwrap() {
                        put(&mut expected, label.0 as u64);
                    }
                }
                9 => writer.remove_prologue_bytes(a as u32, b as u32).unwrap(),
                10 => writer.eh_begin_fde(syms[a as usize]).unwrap(),
                11 => writer.eh_end_fde().unwrap(),
                12 => writer.eh_advance(a).unwrap(),
                13 => writer.eh_write_inst2(a as u8, b, c).unwrap(),
                14 => writer
                    .reloc(syms[a as usize], b as u32, c, d as i64)
                    .unwrap(),
                15 => writer.flush().unwrap(),
                16 => writer.switch_section(sections[a as usize]).unwrap(),
                17 => {
                    let section = writer.get_sec_ref();
                    let assembler = writer.assembler_mut();
                    let symbol = assembler
                        .sym_predef_func(&op.data, SymBinding::Global)
                        .unwrap();
                    assembler.sym_def(symbol, section, a, b).unwrap();
                    put(&mut expected, symbol.id() as u64);
                }
                _ => panic!("unknown operation"),
            }
            state(&writer, &mut expected);
            use std::fmt::Write;
            writeln!(
                &mut records,
                "{}\t{}\t{}\t{}\t{:?}",
                index + 1,
                op.code,
                start,
                expected.len(),
                op.args
            )
            .unwrap();
        }
        writer.flush().unwrap();
        writer.end_module().unwrap();
    }
    assembler.finalize();
    expected.push(255);
    blob(&mut expected, &assembler.build_object_file().unwrap());
    (input, expected, records)
}

fn compare_reference(executable: &Path, work: &Path, input: &[u8], expected: &[u8], case: &str) {
    let path = work.join("input.bin");
    fs::write(&path, input).unwrap();
    let mode = executable.file_name().unwrap().to_string_lossy();
    let output = Command::new(executable)
        .stdin(Stdio::from(fs::File::open(path).unwrap()))
        .output()
        .expect("start actual C++ reference");
    fs::write(work.join(format!("{mode}.stdout")), &output.stdout).unwrap();
    fs::write(work.join(format!("{mode}.stderr")), &output.stderr).unwrap();
    assert!(
        output.status.success(),
        "{case}: C++ reference failed: {}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let mismatch = expected
        .iter()
        .zip(&output.stdout)
        .position(|(a, b)| a != b)
        .or_else(|| {
            (expected.len() != output.stdout.len())
                .then_some(expected.len().min(output.stdout.len()))
        });
    assert!(mismatch.is_none(), "{case}: first mismatch {mismatch:?}, Rust length {}, C++ length {}; see records.tsv for operation boundaries", expected.len(), output.stdout.len());
}

#[test]
fn complete_x64_writer_matches_actual_upstream() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let active = usize::from(tpde_util::asan::ADDRESS_SANITIZER_ACTIVE);
    let work = std::env::var_os("COMPONENT_ARTIFACT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("tpde-x64-writer-{}", std::process::id()))
        })
        .join(format!("x64-writer-{active}"));
    fs::create_dir_all(&work).unwrap();
    let executables = reference_support::load_prepared(root, &work);
    let scripts = scripts();
    assert_eq!(scripts.len(), 7);
    let functions: usize = scripts.iter().map(|script| script.functions).sum();
    assert_eq!(functions, 1259);
    let operations: usize = scripts.iter().map(|script| script.ops.len()).sum();
    let mut comparisons = 0usize;
    let mut failures = vec![];
    let mut roster = String::from("script\tname\tfunctions\toperations\tcpp_mode\tverdict\n");
    for (index, script) in scripts.iter().enumerate() {
        let directory = work.join(format!("case-{index}-{}", script.name));
        fs::create_dir_all(&directory).unwrap();
        let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(script)));
        let Ok((input, expected, records)) = run else {
            failures.push(format!("Rust case {index}: {}", script.name));
            continue;
        };
        fs::write(directory.join("input.bin"), &input).unwrap();
        fs::write(directory.join("expected.bin"), &expected).unwrap();
        fs::write(directory.join("records.tsv"), records).unwrap();
        for executable in &executables {
            let mode = executable.file_name().unwrap().to_string_lossy();
            let name = format!("{}: {mode}", script.name);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                compare_reference(executable, &directory, &input, &expected, &name);
            }));
            let pass = result.is_ok();
            if !pass {
                failures.push(name);
            }
            use std::fmt::Write;
            writeln!(
                &mut roster,
                "{index}\t{}\t{}\t{}\t{mode}\t{}",
                script.name,
                script.functions,
                script.ops.len(),
                if pass { "PASS" } else { "FAIL" }
            )
            .unwrap();
            comparisons += 1;
        }
    }
    fs::write(work.join("roster.tsv"), roster).unwrap();
    checked(
        Command::new("python3").args(["-I", "-B", "-c", "import hashlib,json,pathlib,sys; root=pathlib.Path(sys.argv[1]); excluded={'sha256.json','hash-artifacts.stdout','hash-artifacts.stderr'}; paths=sorted(p for p in root.rglob('*') if p.is_file() and p.name not in excluded); (root/'sha256.json').write_text(json.dumps({str(p.relative_to(root)):hashlib.sha256(p.read_bytes()).hexdigest() for p in paths},indent=2)+'\\n')"]).arg(&work),
        &work, "hash-artifacts",
    );
    assert!(
        failures.is_empty(),
        "X64 writer differential failures: {failures:?}"
    );
    assert_eq!(comparisons, 21);
    println!("TPDE_X64_WRITER_ORACLE PASS scripts=7 cpp_modes=3 functions={functions} operations={operations} comparisons={comparisons} active={active} register_pairs=240 widths=2 exclusions=0");
}

// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![cfg(feature = "upstream-oracle")]

#[path = "../../test-support/oracle.rs"]
mod oracle;

use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
};
use tpde_assembler::{Architecture, AssemblerElf, SecRef, SectionKind, SymBinding, SymRef};
use tpde_core::Reg;
use tpde_function_writer::*;

#[derive(Default)]
struct Target {
    finishes: u64,
}
impl FunctionWriterTarget for Target {
    const CIE: TargetCieInfo<'static> = TargetCieInfo {
        instrs: &[0x0c, 7, 8, 0x90, 1],
        return_addr_register: 16,
        code_alignment_factor: 1,
        data_alignment_factor: 0x78,
    };
    fn handle_fixups(
        &mut self,
        _: &mut FunctionWriterBase<'_, '_, '_>,
    ) -> types_error::PgResult<()> {
        self.finishes += 1;
        Ok(())
    }
}
#[derive(Clone)]
struct Op {
    code: u8,
    args: [u64; 4],
    data: Vec<u8>,
    name: &'static str,
}
fn op(code: u8, args: [u64; 4], name: &'static str) -> Op {
    Op {
        code,
        args,
        data: vec![],
        name,
    }
}
fn bytes(data: Vec<u8>) -> Op {
    Op {
        code: 34,
        args: [0; 4],
        data,
        name: "text bytes",
    }
}
fn table(size: u32, seed: u32) -> Op {
    let mut result = op(
        14,
        [
            size as u64,
            seed as u64 % 256,
            (seed as u64 * 3) % 256,
            (seed % 256) as u64,
        ],
        "jump table allocation",
    );
    for i in 0..size {
        result
            .data
            .extend_from_slice(&(i.wrapping_mul(131) + seed).to_le_bytes());
    }
    result
}
fn put(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn blob(out: &mut Vec<u8>, bytes: &[u8]) {
    put(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}
fn state(w: &FunctionWriterBase<'_, '_, '_>, out: &mut Vec<u8>) {
    for n in [
        w.offset() as u64,
        w.allocated_size() as u64,
        w.func_begin() as u64,
        w.label_skew() as u64,
        w.get_sec_ref().id() as u64,
        w.get_section().unwrap().align as u64,
    ] {
        put(out, n);
    }
}
fn table_state(header: &JumpTableHeader, labels: &[Label], out: &mut Vec<u8>) {
    for n in [
        header.size() as u64,
        header.off as u64,
        header.idx.id() as u64,
        header.tmp.id() as u64,
        header.misc as u64,
    ] {
        put(out, n);
    }
    for label in labels {
        put(out, label.0 as u64);
    }
}
fn integral(w: &mut ModuleWriter<'_, '_, '_, Target>, kind: u64, value: u64, unchecked: bool) {
    macro_rules! write {
        ($ty:ty) => {
            if unchecked {
                w.write_unchecked(value as $ty).unwrap()
            } else {
                w.write(value as $ty).unwrap()
            }
        };
    }
    match kind {
        0 => write!(u8),
        1 => write!(i8),
        2 => write!(u16),
        3 => write!(i16),
        4 => write!(u32),
        5 => write!(i32),
        6 => write!(u64),
        7 => write!(i64),
        8 => {
            if unchecked {
                w.write_unchecked(value != 0).unwrap()
            } else {
                w.write(value != 0).unwrap()
            }
        }
        _ => panic!("integral kind"),
    }
}
fn run(ops: &[Op], architecture: Architecture) -> (Vec<u8>, Vec<u8>, Vec<(usize, usize, String)>) {
    let mut input = vec![if architecture == Architecture::Aarch64 {
        0
    } else {
        1
    }];
    let mut expected = vec![];
    let mut records = vec![];
    let context = mcx::MemoryContext::new("function writer oracle");
    let mut assembler = AssemblerElf::new(context.mcx(), architecture).unwrap();
    let mut syms = [SymRef::default(); 5];
    for (slot, name) in syms[1..].iter_mut().zip([
        b"personality.0" as &[u8],
        b"personality.1",
        b"type.0",
        b"type.1",
    ]) {
        *slot = assembler.sym_add_undef(name, SymBinding::Global).unwrap();
    }
    let secs: [SecRef; 2] = [
        assembler.get_default_section(SectionKind::Text).unwrap(),
        assembler.create_section(SectionKind::Text).unwrap(),
    ];
    let mut writer = FunctionWriter::new(context.mcx(), Target::default());
    {
        let mut w = writer.begin_module(&mut assembler).unwrap();
        for (step, operation) in ops.iter().enumerate() {
            let Op {
                code,
                args: [x, y, z, t],
                data,
                name,
            } = operation;
            let (x, y, z, t) = (*x, *y, *z, *t);
            input.push(*code);
            for arg in [x, y, z, t] {
                put(&mut input, arg);
            }
            input.extend_from_slice(&(data.len() as u32).to_le_bytes());
            input.extend_from_slice(data);
            let start = expected.len();
            expected.push(*code);
            match *code {
                0 => {
                    w.switch_section(secs[x as usize]).unwrap();
                    put(&mut expected, w.get_sec_ref().id() as u64);
                }
                1 => {
                    w.begin_func(x as u32, y as u32).unwrap();
                    state(&w, &mut expected);
                }
                2 | 3 => {
                    integral(&mut w, x, y, *code == 3);
                    put(&mut expected, w.offset() as u64);
                }
                4 => {
                    w.ensure_space(x as usize).unwrap();
                    state(&w, &mut expected);
                }
                5 => {
                    w.align(x as usize).unwrap();
                    state(&w, &mut expected);
                }
                6 => put(&mut expected, w.label_create().unwrap().0 as u64),
                7 => put(
                    &mut expected,
                    w.label_is_pending(Label(x as u32)).unwrap() as u64,
                ),
                8 => w.label_place(Label(x as u32), y as u32).unwrap(),
                9 => put(
                    &mut expected,
                    w.label_offset(Label(x as u32)).unwrap() as u64,
                ),
                10 => w
                    .label_ref(Label(x as u32), y as u32, LabelFixupKind(z as u8))
                    .unwrap(),
                11 => {
                    put(&mut expected, w.label_fixups().len() as u64);
                    for f in w.label_fixups() {
                        for n in [f.label.0 as u64, f.off as u64, f.kind.0 as u64] {
                            put(&mut expected, n);
                        }
                    }
                }
                12 => {
                    w.remove_prologue_bytes(x as u32, y as u32).unwrap();
                    state(&w, &mut expected);
                }
                13 => w.reloc(syms[x as usize], y as u32, z, t as i64).unwrap(),
                14 => {
                    let mut jt = w
                        .alloc_jump_table(x as u32, Reg::new(y), Reg::new(z))
                        .unwrap();
                    jt.set_misc(t as u8);
                    for (label, chunk) in jt.labels_mut().iter_mut().zip(data.chunks_exact(4)) {
                        *label = Label(u32::from_le_bytes(chunk.try_into().unwrap()));
                    }
                    table_state(&jt, jt.labels(), &mut expected);
                }
                15 => {
                    put(&mut expected, w.jump_table_count() as u64);
                    for i in 0..w.jump_table_count() {
                        table_state(
                            w.jump_table_header(i).unwrap(),
                            w.jump_table_labels(i).unwrap(),
                            &mut expected,
                        );
                    }
                }
                16 => put(
                    &mut expected,
                    w.get_jump_table_section().unwrap().id() as u64,
                ),
                17 => w.eh_begin_fde(syms[x as usize]).unwrap(),
                18 => match x {
                    0 => w.eh_write_inst(y as u8).unwrap(),
                    1 => w.eh_write_inst1(y as u8, z).unwrap(),
                    2 => w.eh_write_inst2(y as u8, z, t).unwrap(),
                    _ => panic!("instruction arity"),
                },
                19 => {
                    if x == 0 {
                        w.eh_advance_raw(y).unwrap()
                    } else {
                        w.eh_advance(y).unwrap()
                    }
                }
                20 => w.eh_align_frame().unwrap(),
                21 => w.eh_end_fde().unwrap(),
                22 => w
                    .except_add_call_site(x as u32, y as u32, Label(z as u32), t != 0)
                    .unwrap(),
                23 => w.except_add_cleanup_action().unwrap(),
                24 => w.except_add_action(x != 0, syms[y as usize]).unwrap(),
                25 => w.except_add_empty_spec_action(x != 0).unwrap(),
                26 => put(
                    &mut expected,
                    w.except_type_idx_for_sym(syms[x as usize]).unwrap() as u64,
                ),
                27 => w.except_encode_func().unwrap(),
                28 => {
                    w.finish_func().unwrap();
                    put(&mut expected, w.target().finishes);
                }
                29 => {
                    w.flush().unwrap();
                    put(&mut expected, w.offset() as u64);
                    put(&mut expected, w.get_section().unwrap().data.len() as u64);
                }
                30 => w.end_module().unwrap(),
                31 => blob(&mut expected, w.data().unwrap()),
                32 => {
                    let mut buffer = [0; 21];
                    let len = if x == 1 {
                        FunctionWriterBase::write_eh_inst1(&mut buffer, y as u8, z).unwrap()
                    } else {
                        FunctionWriterBase::write_eh_inst2(&mut buffer, y as u8, z, t).unwrap()
                    };
                    blob(&mut expected, &buffer[..len]);
                }
                33 => {
                    w.base.begin_func().unwrap();
                    state(&w, &mut expected);
                }
                34 => {
                    w.ensure_space(data.len()).unwrap();
                    for b in data {
                        w.write_unchecked(*b).unwrap();
                    }
                    put(&mut expected, w.offset() as u64);
                }
                _ => panic!("unknown operation {code}"),
            }
            records.push((
                start,
                expected.len(),
                format!(
                    "operation {}: {} (opcode {}, args {:?})",
                    step + 1,
                    name,
                    code,
                    [x, y, z, t]
                ),
            ));
        }
    }
    assembler.finalize();
    let start = expected.len();
    expected.push(255);
    blob(&mut expected, &assembler.build_object_file().unwrap());
    records.push((start, expected.len(), "final ELF image".into()));
    (input, expected, records)
}

fn cases() -> Vec<(&'static str, Vec<Op>)> {
    let mut cases = vec![(
        "empty module",
        vec![
            op(0, [0; 4], "switch text"),
            op(30, [0; 4], "end empty module"),
        ],
    )];
    for seed in 0..4u64 {
        let mut ops = vec![
            op(0, [0; 4], "switch text"),
            bytes(vec![0xab; 3]),
            op(1, [1 << seed, 1 + seed, 0, 0], "begin aligned function"),
        ];
        for round in 0..3u64 {
            ops.push(op(4, [256 + round * 129, 0, 0, 0], "ensure growth"));
            for kind in 0..9u64 {
                ops.push(op(
                    2,
                    [kind, u64::MAX.wrapping_sub(kind * 131 + seed), 0, 0],
                    "integral write",
                ));
                ops.push(op(
                    3,
                    [kind, kind + round, 0, 0],
                    "unchecked integral write",
                ));
            }
        }
        ops.push(op(5, [64, 0, 0, 0], "align after writes"));
        ops.push(bytes(
            (0..(257 + seed * 19))
                .map(|i| (i * 31 + seed) as u8)
                .collect(),
        ));
        for i in 0..80u64 {
            ops.push(op(6, [0; 4], "create label"));
            ops.push(op(7, [i, 0, 0, 0], "pending label"));
            ops.push(op(8, [i, 512 + i, 0, 0], "place label"));
            ops.push(op(9, [i, 0, 0, 0], "query placed label"));
            ops.push(op(10, [i, 512 + i, i % 3, 0], "label fixup"));
        }
        ops.push(op(11, [0; 4], "all fixup fields"));
        for (i, size) in [0, 1, 2, 64, 128, 1025].into_iter().enumerate() {
            ops.push(table(size, (i as u32 + seed as u32) * 7));
        }
        ops.push(op(15, [0; 4], "all jump tables"));
        ops.push(op(16, [0; 4], "jump table section"));
        ops.push(op(27, [0; 4], "exceptions without personality"));
        ops.push(op(28, [0; 4], "finish callback"));
        ops.push(op(29, [0; 4], "flush"));
        ops.push(op(31, [0; 4], "text snapshot"));
        ops.push(op(0, [1, 0, 0, 0], "switch alternate section"));
        ops.push(op(1, [32, 512, 0, 0], "begin second section function"));
        ops.push(op(15, [0; 4], "jump tables reset"));
        ops.push(op(11, [0; 4], "fixups reset"));
        ops.push(bytes(vec![0x5e; 129]));
        ops.push(op(29, [0; 4], "flush second section"));
        ops.push(op(31, [0; 4], "second section snapshot"));
        ops.push(op(0, [0; 4], "switch back"));
        ops.push(op(33, [0; 4], "base begin function"));
        ops.push(op(6, [0; 4], "label index reset"));
        ops.push(op(7, [0; 4], "reset label pending"));
        ops.push(bytes(vec![0x77; 17]));
        ops.push(op(29, [0; 4], "final flush"));
        ops.push(op(30, [0; 4], "end module"));
        cases.push(("text and ownership reuse", ops));
    }
    let mut ops = vec![
        op(0, [0; 4], "switch text"),
        op(1, [1, 0, 0, 0], "zero expected size"),
    ];
    for len in [0, 1, 255, 256, 257, 4096, 65537] {
        ops.push(bytes((0..len).map(|i| (i * 17) as u8).collect()));
        ops.push(op(4, [17, 0, 0, 0], "post growth capacity"));
    }
    ops.extend([
        op(29, [0; 4], "flush large function"),
        op(31, [0; 4], "large text snapshot"),
        op(30, [0; 4], "end large module"),
    ]);
    cases.push(("growth boundaries", ops));
    for personality in [0u64, 1, 2] {
        let mut ops = vec![op(0, [0; 4], "switch text")];
        let mut offset = 0;
        for (index, personality) in [personality, personality, 1, 2, 0, 1]
            .into_iter()
            .enumerate()
        {
            let align = 1u64 << (index % 5);
            offset = (offset + align - 1) & !(align - 1);
            let begin = offset;
            let size = 127 + index as u64 * 97;
            offset += size;
            ops.push(op(1, [align, 16, 0, 0], "begin EH function"));
            ops.push(op(
                17,
                [personality, 0, 0, 0],
                "begin FDE personality switch",
            ));
            ops.push(bytes(
                (0..size).map(|i| (i * 19 + index as u64) as u8).collect(),
            ));
            for (arity, opcode, a, b) in [
                (0, 0, 0, 0),
                (1, 0x80, 63, 0),
                (1, 0x0e, 0, 0),
                (1, 0x0e, 127, 0),
                (1, 0x0e, 128, 0),
                (1, 0x0e, u64::MAX, 0),
                (2, 0x0c, 7, 8),
                (2, 0x80, 3, u64::MAX),
            ] {
                ops.push(op(18, [arity, opcode, a, b], "EH instruction"));
                ops.push(op(20, [0; 4], "explicit EH alignment"));
            }
            for (i, units) in [0, 1, 63, 64, 255, 256, 65535, 65536, u32::MAX as u64]
                .into_iter()
                .enumerate()
            {
                ops.push(op(19, [(i % 2) as u64, units, 0, 0], "EH advance boundary"));
            }
            for i in 0..3u64 {
                ops.push(op(6, [0; 4], "landing label"));
                ops.push(op(8, [i, begin + size - 8 + i, 0, 0], "place landing pad"));
            }
            ops.push(op(
                22,
                [begin + 16, 5, 0, 1],
                "cleanup call site with leading gap",
            ));
            ops.push(op(22, [begin + 32, 9, 1, 0], "typed call site"));
            ops.push(op(24, [1, 3, 0, 0], "first typed action"));
            ops.push(op(24, [0, 4, 0, 0], "chained second type"));
            ops.push(op(24, [0, 3, 0, 0], "deduplicated type"));
            ops.push(op(24, [0, 0, 0, 0], "catch all"));
            ops.push(op(26, [3, 0, 0, 0], "first type index"));
            ops.push(op(26, [4, 0, 0, 0], "second type index"));
            ops.push(op(25, [0; 4], "empty spec action"));
            ops.push(op(25, [0; 4], "reused empty spec"));
            ops.push(op(23, [0; 4], "cleanup chain terminator"));
            ops.push(op(22, [begin + 48, 3, 2, 0], "new action chain call site"));
            ops.push(op(24, [1, 0, 0, 0], "new catch all chain"));
            ops.push(op(
                13,
                [3, 0x1234, begin + 24, u64::MAX - 3],
                "function relocation",
            ));
            ops.push(op(
                12,
                [begin, if index % 2 == 0 { 0 } else { 4 }, 0, 0],
                "prologue truncation",
            ));
            if index % 2 != 0 {
                offset -= 4;
            }
            for i in 0..3 {
                ops.push(op(9, [i, 0, 0, 0], "skewed landing label"));
            }
            ops.push(op(27, [0; 4], "encode exception tables"));
            ops.push(op(21, [0; 4], "finish FDE"));
            ops.push(op(28, [0; 4], "finish function callback"));
        }
        ops.extend([
            op(29, [0; 4], "flush EH text"),
            op(31, [0; 4], "EH text snapshot"),
            op(30, [0; 4], "end EH module"),
        ]);
        cases.push(("EH personality and LSDA", ops));
    }
    let mut ops = vec![op(0, [0; 4], "switch text")];
    for opcode in [0u64, 1, 0x0c, 0x0e, 0x3f, 0x40, 0x80, 0xc0] {
        for arg in [
            0,
            1,
            2,
            63,
            127,
            128,
            256,
            512,
            513,
            16383,
            16384,
            u32::MAX as u64,
            u64::MAX,
        ] {
            if opcode & 0xc0 != 0 && arg & 0xc0 != 0 {
                continue;
            }
            for second in [0, 1, 127, 128, u64::MAX] {
                ops.push(op(
                    32,
                    [2, opcode, arg, second],
                    "static two argument instruction",
                ));
            }
            ops.push(op(
                32,
                [1, opcode, arg, 0],
                "static one argument instruction",
            ));
        }
    }
    ops.push(op(30, [0; 4], "end static module"));
    cases.push(("static instruction forms", ops));
    let mut ops = vec![
        op(0, [0; 4], "switch text"),
        op(1, [1, 8, 0, 0], "begin empty LSDA"),
        op(17, [1, 0, 0, 0], "personality without sites"),
        bytes(vec![0; 1]),
        op(27, [0; 4], "encode no sites"),
        op(21, [0; 4], "end FDE"),
        op(1, [1, 8, 0, 0], "begin exact span LSDA"),
        op(17, [1, 0, 0, 0], "reuse personality"),
        bytes(vec![0x55; 64]),
        op(6, [0; 4], "exact span landing pad"),
        op(8, [0, 33, 0, 0], "place exact span landing"),
        op(22, [1, 32, 0, 0], "call site starts at function begin"),
        op(24, [1, 3, 0, 0], "begin long action chain"),
    ];
    for _ in 0..80 {
        ops.push(op(24, [0, 3, 0, 0], "long deduplicated action chain"));
    }
    ops.extend([
        op(23, [0; 4], "multi-byte negative cleanup offset"),
        op(
            22,
            [33, 32, 0, 1],
            "contiguous call site reaches function end",
        ),
        op(27, [0; 4], "encode exact span LSDA"),
        op(21, [0; 4], "end exact span FDE"),
        op(29, [0; 4], "flush LSDA edge text"),
        op(30, [0; 4], "end LSDA edge module"),
    ]);
    cases.push(("LSDA call site boundaries", ops));
    cases
}

fn compare(
    executable: &Path,
    work: &Path,
    input: &[u8],
    expected: &[u8],
    records: &[(usize, usize, String)],
    case: &str,
) {
    let path = work.join("input.bin");
    fs::write(&path, input).unwrap();
    let output = Command::new(executable)
        .stdin(Stdio::from(fs::File::open(path).unwrap()))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{case}: C++ oracle failed: {}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    if expected != output.stdout {
        let byte = expected
            .iter()
            .zip(&output.stdout)
            .position(|(a, b)| a != b)
            .unwrap_or(expected.len().min(output.stdout.len()));
        let record = records
            .iter()
            .find(|(start, end, _)| *start <= byte && byte < *end);
        let local = record.map(|(start, _, _)| byte - start).unwrap_or(0);
        let low = byte.saturating_sub(16);
        let high = (byte + 17).min(expected.len()).min(output.stdout.len());
        panic!("{case}: mismatch at byte {byte}, record {record:?}, record offset {local}, Rust length {}, C++ length {}; Rust nearby {:02x?}, C++ nearby {:02x?}",expected.len(),output.stdout.len(),&expected[low..high],&output.stdout[low..high]);
    }
}
#[test]
fn upstream_function_writer() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let work = std::env::temp_dir().join(format!("tpde-function-writer-{}", std::process::id()));
    fs::create_dir_all(&work).unwrap();
    let reference = root.join("tests/upstream");
    let sources = vec![
        root.join("tests/oracle.cpp"),
        reference.join("tpde/src/FunctionWriter.cpp"),
        reference.join("tpde/src/Assembler.cpp"),
        reference.join("tpde/src/AssemblerElf.cpp"),
        reference.join("tpde/src/StringTable.cpp"),
        reference.join("tpde/src/util/SmallVector.cpp"),
    ];
    let mut executables = vec![];
    for (name, assertions, flags) in [
        ("debug", true, vec![]),
        ("ndebug", false, vec![]),
        (
            "sanitizers",
            true,
            vec![
                "-fsanitize=address,undefined",
                "-fno-sanitize-recover=all",
                "-fno-omit-frame-pointer",
            ],
        ),
    ] {
        let executable = work.join(name);
        oracle::compile_cpp_with_flags(
            &reference.join("tpde/include"),
            &executable,
            &sources,
            assertions,
            &flags,
        );
        executables.push(executable);
    }
    let cases = cases();
    let mut operations = 0;
    for (index, (name, ops)) in cases.iter().enumerate() {
        for architecture in [Architecture::Aarch64, Architecture::X86_64] {
            let (input, expected, records) = run(ops, architecture);
            operations += ops.len();
            for executable in &executables {
                compare(
                    executable,
                    &work,
                    &input,
                    &expected,
                    &records,
                    &format!(
                        "script {index}: {name}, {architecture:?}, {}",
                        executable.file_name().unwrap().to_string_lossy()
                    ),
                );
            }
        }
    }
    println!("TPDE_FUNCTION_WRITER_ORACLE PASS scripts={} architectures=2 cpp_modes=3 active={} operations={}",cases.len(),usize::from(tpde_util::asan::ADDRESS_SANITIZER_ACTIVE),operations);
    fs::remove_dir_all(work).unwrap();
}

// SPDX-FileCopyrightText: 2018 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

use crate::Result;
use crate::model::{Entry, Opcode};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(i8)]
enum Kind {
    Root = -1,
    Prefix = 0x10,
    Instr = 0x20,
    WeakInstr = 0x30,
    Table16 = 0x01,
    Table8E = 0x11,
    Escape = 0x02,
    Table256 = 0x12,
    Vex = 0x22,
    MandatoryPrefix = 0x03,
}

impl Kind {
    const ORDER: [Self; 7] = [
        Self::Root,
        Self::Escape,
        Self::Table256,
        Self::MandatoryPrefix,
        Self::Table16,
        Self::Table8E,
        Self::Vex,
    ];

    fn is_table(self) -> bool {
        !matches!(self, Self::Prefix | Self::Instr | Self::WeakInstr)
    }

    fn len(self) -> usize {
        match self {
            Self::Root | Self::Table256 => 256,
            Self::Escape | Self::Table8E | Self::Vex => 8,
            Self::MandatoryPrefix => 4,
            Self::Table16 => 16,
            _ => unreachable!(),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Root => "TABLE_ROOT",
            Self::Escape => "ESCAPE",
            Self::Table256 => "TABLE256",
            Self::MandatoryPrefix => "TABLE_PREFIX",
            Self::Table16 => "TABLE16",
            Self::Table8E => "TABLE8E",
            Self::Vex => "TABLE_VEX",
            _ => unreachable!(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Element {
    kind: Kind,
    value: usize,
}

type Table = Vec<Option<Element>>;
type OpcodePath = [Option<Vec<usize>>; 7];

struct Trie {
    tables: Vec<Option<Table>>,
    kinds: BTreeMap<Kind, Vec<usize>>,
    kind_order: Vec<Kind>,
}

impl Trie {
    fn new(root_count: usize) -> Self {
        Self {
            tables: vec![Some(vec![None; root_count])],
            kinds: BTreeMap::new(),
            kind_order: Vec::new(),
        }
    }

    fn table(&self, index: usize) -> &Table {
        self.tables[index].as_ref().expect("live trie table")
    }

    fn table_mut(&mut self, index: usize) -> &mut Table {
        self.tables[index].as_mut().expect("live trie table")
    }

    fn register_kind(&mut self, kind: Kind) {
        if !self.kinds.contains_key(&kind) {
            self.kinds.insert(kind, Vec::new());
            self.kind_order.push(kind);
        }
    }

    fn add_table(&mut self, kind: Kind) -> usize {
        let index = self.tables.len();
        self.tables.push(Some(vec![None; kind.len()]));
        self.register_kind(kind);
        self.kinds.get_mut(&kind).unwrap().push(index);
        index
    }

    fn clone_element(&mut self, element: Option<Element>) -> Option<Element> {
        let value = element?;
        if !value.kind.is_table() {
            return element;
        }
        let index = self.add_table(value.kind);
        for i in 0..self.table(value.value).len() {
            let original = self.table(value.value)[i];
            let cloned = self.clone_element(original);
            self.table_mut(index)[i] = cloned;
        }
        Some(Element {
            kind: value.kind,
            value: index,
        })
    }

    fn transform_opcode(opcode: &Opcode) -> Result<OpcodePath> {
        if opcode.vex > 2 || opcode.escape > 7 {
            return Err("invalid opcode escape or VEX class".into());
        }
        if opcode.modrm.1.is_some_and(|v| v > 7) || opcode.modrm.2.is_some_and(|v| v > 7) {
            return Err("invalid ModRM selector".into());
        }
        let extended = opcode.extended && opcode.modrm.2.is_none();
        let end = usize::from(opcode.opc) + if extended { 8 } else { 1 };
        if end > 256 {
            return Err("extended opcode exceeds byte range".into());
        }
        let mut opc = Some((usize::from(opcode.opc)..end).collect());
        if opcode.escape == 0 && matches!(opcode.opc, 0xc4 | 0xc5 | 0x62) {
            if opcode.prefix.is_some()
                || opcode.modrm != (Some('m'), None, None)
                || opcode.rexw.is_some()
                || opcode.vexl.is_some()
            {
                return Err("invalid legacy opcode sharing a VEX/EVEX prefix".into());
            }
            return Ok([opc, Some(vec![0]), None, None, None, None, None]);
        }
        let (root, escape) = if opcode.escape == 0 {
            (opc.take(), None)
        } else {
            let roots = match opcode.vex {
                0 => vec![0x0f],
                1 => vec![0xc4, 0xc5],
                2 => vec![0x62],
                _ => unreachable!(),
            };
            (Some(roots), Some(vec![usize::from(opcode.escape)]))
        };
        let prefix = match opcode.prefix.as_deref() {
            None | Some("") => None,
            Some("NFx") => Some(vec![0, 1]),
            Some("NP") => Some(vec![0]),
            Some("66") => Some(vec![1]),
            Some("F3") => Some(vec![2]),
            Some("F2") => Some(vec![3]),
            Some(value) => return Err(format!("invalid mandatory prefix {value}")),
        };
        let (table16, table8e) = if opcode.modrm != (None, None, None) {
            let mods: &[usize] = match opcode.modrm.0 {
                Some('m') => &[0],
                Some('r') => &[1],
                None => &[0, 1],
                Some(_) => return Err("invalid ModRM mode".into()),
            };
            let regs = opcode
                .modrm
                .1
                .map_or_else(|| (0..8).collect(), |r| vec![usize::from(r)]);
            let mut indices = Vec::with_capacity(mods.len() * regs.len());
            for m in mods {
                for reg in &regs {
                    indices.push(*m + (reg << 1));
                }
            }
            let rm = opcode
                .modrm
                .2
                .filter(|_| !opcode.extended)
                .map(|v| vec![usize::from(v)]);
            (Some(indices), rm)
        } else {
            (None, None)
        };
        let vector_length = opcode
            .vexl
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("IG");
        let vex = if opcode.rexw.is_some() || vector_length != "IG" {
            let widths: &[usize] = match opcode.rexw.as_deref() {
                None => &[0, 1],
                Some("0") => &[0],
                Some("1") => &[1],
                Some(value) => return Err(format!("invalid REX.W {value}")),
            };
            let lengths: &[usize] = match (opcode.vex < 2, vector_length) {
                (_, "0") => &[0],
                (true, "1") => &[2],
                (true, "IG") => &[0, 2],
                (false, "12") => &[2, 4],
                (false, "2") => &[4],
                (false, "IG") => &[0, 2, 4, 6],
                _ => return Err(format!("invalid vector length {vector_length}")),
            };
            let mut indices = Vec::with_capacity(widths.len() * lengths.len());
            for width in widths {
                for length in lengths {
                    indices.push(width + length);
                }
            }
            Some(indices)
        } else {
            None
        };
        Ok([root, escape, opc, prefix, table16, table8e, vex])
    }

    fn add_opcode(&mut self, opcode: &Opcode, desc: usize, root: usize, weak: bool) -> Result<()> {
        let path = Self::transform_opcode(opcode)?;
        let mut frontier = vec![(0, root)];
        for (kind, mut indices) in Kind::ORDER.into_iter().zip(path) {
            let mut next = Vec::new();
            for (table, slot) in frontier {
                let original = self
                    .table(table)
                    .get(slot)
                    .copied()
                    .ok_or_else(|| "opcode trie index out of range".to_string())?;
                if indices.is_none() {
                    if original.is_none_or(|element| element.kind != kind) {
                        next.push((table, slot));
                        continue;
                    }
                    // This expansion persists across the remaining frontier.
                    indices = Some((0..kind.len()).collect());
                }
                let child = if let Some(element) = original.filter(|element| element.kind == kind) {
                    element.value
                } else {
                    let child = self.add_table(kind);
                    if original.is_some() {
                        self.table_mut(child)[0] = original;
                        for i in 1..kind.len() {
                            let cloned = self.clone_element(original);
                            self.table_mut(child)[i] = cloned;
                        }
                    }
                    self.table_mut(table)[slot] = Some(Element { kind, value: child });
                    child
                };
                for slot in indices.as_ref().unwrap() {
                    next.push((child, *slot));
                }
            }
            frontier = next;
        }
        let value = desc
            .checked_mul(4)
            .ok_or_else(|| "descriptor index overflow".to_string())?;
        for (table, slot) in frontier {
            let entry = &mut self.table_mut(table)[slot];
            if entry.is_none_or(|element| element.kind == Kind::WeakInstr) {
                *entry = Some(Element {
                    kind: if weak { Kind::WeakInstr } else { Kind::Instr },
                    value,
                });
            } else if !weak {
                return Err(format!("redundant non-weak opcode {opcode:?}"));
            }
        }
        Ok(())
    }

    fn add_prefix(&mut self, byte: usize, prefix: usize, root: usize) {
        if self.table(0)[root].is_none() {
            let value = self.add_table(Kind::Root);
            self.table_mut(0)[root] = Some(Element {
                kind: Kind::Root,
                value,
            });
        }
        let table = self.table(0)[root].unwrap().value;
        self.table_mut(table)[byte] = Some(Element {
            kind: Kind::Prefix,
            value: prefix,
        });
    }

    fn deduplicate(&mut self) {
        let mut synonyms: BTreeMap<usize, Option<Element>> = BTreeMap::new();
        for kind in Kind::ORDER.into_iter().rev() {
            self.register_kind(kind);
            let mut entries = BTreeMap::new();
            for index in self.kinds[&kind].clone() {
                let table = self.table_mut(index);
                for element in table.iter_mut() {
                    if let Some(value) = *element {
                        if value.kind.is_table() {
                            if let Some(synonym) = synonyms.get(&value.value) {
                                *element = *synonym;
                            }
                        }
                    }
                }
                if table.iter().all(|element| *element == table[0]) {
                    synonyms.insert(index, table[0]);
                    self.tables[index] = None;
                } else if let Some(original) = entries.get(table) {
                    synonyms.insert(
                        index,
                        Some(Element {
                            kind,
                            value: *original,
                        }),
                    );
                    self.tables[index] = None;
                } else {
                    entries.insert(table.clone(), index);
                }
            }
        }
    }

    fn compile(&self) -> Result<(Vec<u16>, Vec<u16>)> {
        let mut offsets = vec![None; self.tables.len()];
        let mut length = 0usize;
        for (index, entry) in self.tables.iter().enumerate().skip(1) {
            if let Some(table) = entry {
                if !table.is_empty() {
                    offsets[index] = Some(length);
                    length = length
                        .checked_add((table.len() + 3) & !3)
                        .ok_or_else(|| "table size overflow".to_string())?;
                }
            }
        }
        if length >= 0x8000 {
            return Err(format!("maximum table size exceeded: {length:#x}"));
        }
        let mut data = vec![0; length];
        for (index, offset) in offsets.iter().enumerate() {
            let Some(offset) = *offset else { continue };
            for (i, entry) in self.table(index).iter().enumerate() {
                if let Some(entry) = entry {
                    let value = if entry.kind.is_table() {
                        offsets[entry.value]
                            .ok_or_else(|| "unresolved deduplicated table".to_string())?
                    } else {
                        entry.value
                    };
                    data[offset + i] = u16::try_from(value | ((entry.kind as u8 & 3) as usize))
                        .map_err(|_| "table value exceeds u16".to_string())?;
                }
            }
        }
        let roots = self
            .table(0)
            .iter()
            .map(|root| {
                let root = root.ok_or_else(|| "missing opcode root".to_string())?;
                let offset = offsets[root.value].ok_or_else(|| {
                    "duplicate decode modes produce a deduplicated root".to_string()
                })?;
                Ok(offset as u16)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok((data, roots))
    }

    fn stats(&self) -> String {
        let mut result = String::from("{");
        for (i, kind) in self.kind_order.iter().enumerate() {
            if i != 0 {
                result.push_str(", ");
            }
            let count = self.kinds[kind]
                .iter()
                .filter(|&&index| self.tables[index].is_some())
                .count();
            write!(result, "'{}': {count}", kind.name()).unwrap();
        }
        result.push('}');
        result
    }
}

pub fn superstring(strings: &[String]) -> String {
    let mut ordered: Vec<&str> = strings.iter().map(String::as_str).collect();
    ordered.sort_by_key(|value| std::cmp::Reverse(value.chars().count()));
    let mut real: Vec<&str> = Vec::new();
    for value in ordered {
        if !real.iter().any(|existing| existing.contains(value)) {
            real.push(value);
        }
    }
    real.sort();
    let mut merged = String::new();
    while !real.is_empty() {
        let suffix_starts: Vec<usize> = merged
            .char_indices()
            .rev()
            .take(16)
            .map(|(i, _)| i)
            .collect();
        let mut overlap = None;
        for start in suffix_starts.into_iter().rev() {
            let suffix = &merged[start..];
            let index = real.partition_point(|value| *value < suffix);
            if index < real.len() && real[index].starts_with(suffix) {
                overlap = Some((index, suffix.len()));
                break;
            }
        }
        if let Some((index, size)) = overlap {
            merged.push_str(&real.remove(index)[size..]);
        } else {
            merged.push_str(real.pop().unwrap());
        }
    }
    merged
}

fn canonical_mnemonic(value: &str) -> &str {
    match value {
        "PUSH_SEG" => "PUSH",
        "POP_SEG" => "POP",
        "MOV_CR2G" | "MOV_G2CR" => "MOV_CR",
        "MOV_DR2G" | "MOV_G2DR" => "MOV_DR",
        "MMX_MOVD_M2G" | "MMX_MOVD_G2M" => "MMX_MOVD",
        "MMX_MOVQ_M2G" | "MMX_MOVQ_G2M" => "MMX_MOVQ",
        "SSE_MOVD_X2G" | "SSE_MOVD_G2X" => "SSE_MOVD",
        "SSE_MOVQ_X2G" | "SSE_MOVQ_G2X" => "SSE_MOVQ",
        "VMOVD_X2G" | "VMOVD_G2X" => "VMOVD",
        "VMOVQ_X2G" | "VMOVQ_G2X" => "VMOVQ",
        _ => value,
    }
}

fn intel_mnemonic(value: &str) -> String {
    let mut result = value.to_string();
    for (from, to) in [
        ("SSE_", ""),
        ("MMX_", ""),
        ("EVX_", "V"),
        ("MOVABS", "MOV"),
        ("RESERVED_", ""),
        ("JMPF", "JMP FAR"),
        ("CALLF", "CALL FAR"),
        ("_S2G", ""),
        ("_G2S", ""),
        ("_X2G", ""),
        ("_G2X", ""),
        ("_CR", ""),
        ("_DR", ""),
        ("REP_", "REP "),
        ("CMPXCHGD", "CMPXCHG"),
        ("JCXZ", "JCXZ JECXZJRCXZ"),
        ("C_SEP", "CWD CDQ CQO"),
        ("C_EX", "CBW CWDECDQE"),
        ("XCHG_NOP", ""),
    ] {
        result = result.replace(from, to);
    }
    result.to_lowercase()
}

#[derive(Debug)]
pub struct DecodeOutput {
    pub public_rs: String,
    pub private_rs: String,
    pub semantic_tsv: String,
    pub stats: String,
}

pub fn generate(entries: &[Entry], modes: &[u8]) -> Result<DecodeOutput> {
    if modes.iter().any(|mode| !matches!(mode, 32 | 64)) {
        return Err("decode mode must be 32 or 64".into());
    }
    let mut trie = Trie::new(modes.len());
    for (root, &mode) in modes.iter().enumerate() {
        for (byte, prefix) in [
            (0x66, 0xfffa),
            (0x67, 0xfffb),
            (0xf0, 0xfffc),
            (0xf2, 0xfffd),
            (0xf3, 0xfffd),
            (0x64, 0xfff9),
            (0x65, 0xfff9),
        ] {
            trie.add_prefix(byte, prefix, root);
        }
        for byte in [0x26, 0x2e, 0x36, 0x3e] {
            trie.add_prefix(byte, 0xfff8 + usize::from(mode <= 32), root);
        }
        if mode > 32 {
            for byte in 0x40..0x50 {
                trie.add_prefix(byte, 0xfffe, root);
            }
        }
    }
    let mut mnemonics = BTreeSet::from(["PAUSE"]);
    let mut descriptors = vec![("", [0; 3])];
    let mut descriptor_map = BTreeMap::new();
    for entry in entries {
        let mnemonic = canonical_mnemonic(&entry.desc.mnemonic);
        if !mnemonic
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(format!(
                "invalid generated mnemonic identifier {mnemonic:?}"
            ));
        }
        mnemonics.insert(mnemonic);
        let ign66 = matches!(
            entry.opcode.prefix.as_deref(),
            Some("NP" | "66" | "F2" | "F3")
        );
        let modrm = entry.opcode.modrm != (None, None, None);
        let words = entry
            .desc
            .encode(mnemonic, ign66, modrm)
            .map_err(|error| format!("line {}: {error}", entry.line))?;
        let key = (mnemonic, words);
        let descriptor = if let Some(&index) = descriptor_map.get(&key) {
            index
        } else {
            let index = descriptors.len();
            descriptor_map.insert(key, index);
            descriptors.push(key);
            index
        };
        for (root, &mode) in modes.iter().enumerate() {
            if !entry
                .desc
                .flags
                .contains(if mode <= 32 { "O64" } else { "I64" })
            {
                trie.add_opcode(&entry.opcode, descriptor, root, entry.weak)
                    .map_err(|error| format!("line {}: {error}", entry.line))?;
            }
        }
    }
    trie.deduplicate();
    let (table, roots) = trie.compile()?;
    let mnemonics: Vec<&str> = mnemonics.into_iter().collect();
    let intel_names: Vec<String> = mnemonics.iter().map(|m| intel_mnemonic(m)).collect();
    let string_table = superstring(&intel_names);
    let mut public_rs = String::new();
    let mut private_rs = String::new();
    let mut semantic_tsv = String::new();
    for (index, mnemonic) in mnemonics.iter().enumerate() {
        let index = u16::try_from(index).map_err(|_| "too many mnemonics".to_string())?;
        writeln!(public_rs, "pub const FDI_{mnemonic}: u16 = {index};").unwrap();
    }
    writeln!(
        public_rs,
        "pub const FD_MNEMONICS: &[&str] = &{mnemonics:?};"
    )
    .unwrap();
    writeln!(
        private_rs,
        "pub const FD_DECODE_TABLE_DATA: &[u16] = &{table:?};"
    )
    .unwrap();
    for (index, value) in table.iter().enumerate() {
        writeln!(semantic_tsv, "TABLE\t{index}\t{value}").unwrap();
    }
    private_rs.push_str("pub const FD_DECODE_TABLE_DESCS: &[[u16; 4]] = &[\n");
    for (index, &(mnemonic, words)) in descriptors.iter().enumerate() {
        let id = if index == 0 {
            0
        } else {
            mnemonics.binary_search(&mnemonic).unwrap()
        };
        let [a, b, c] = words;
        writeln!(private_rs, "    [{id}, {a}, {b}, {c}],").unwrap();
        writeln!(semantic_tsv, "DESC\t{index}\t{id}\t{a}\t{b}\t{c}").unwrap();
    }
    private_rs.push_str("];\n");
    writeln!(
        private_rs,
        "pub const FD_DECODE_TABLE_STRTAB1: &str = {string_table:?};"
    )
    .unwrap();
    let mut name_offsets = Vec::new();
    let mut name_lengths = Vec::new();
    for (id, (mnemonic, intel)) in mnemonics.iter().zip(&intel_names).enumerate() {
        let offset = string_table.find(intel).unwrap();
        name_offsets.push(
            u16::try_from(offset).map_err(|_| "mnemonic string offset exceeds u16".to_string())?,
        );
        name_lengths
            .push(u8::try_from(intel.len()).map_err(|_| "mnemonic length exceeds u8".to_string())?);
        writeln!(
            semantic_tsv,
            "MNEM\t{id}\t{mnemonic}\t{intel}\t{offset}\t{}",
            intel.len()
        )
        .unwrap();
    }
    writeln!(semantic_tsv, "STRTAB\t{string_table}").unwrap();
    writeln!(
        private_rs,
        "pub const FD_DECODE_TABLE_STRTAB2: &[u16] = &{name_offsets:?};"
    )
    .unwrap();
    writeln!(
        private_rs,
        "pub const FD_DECODE_TABLE_STRTAB3: &[u8] = &{name_lengths:?};"
    )
    .unwrap();
    let mode_offsets: Vec<(u8, u16)> = modes.iter().copied().zip(roots).collect();
    writeln!(
        private_rs,
        "pub const FD_TABLE_ROOTS: &[(u8, u16)] = &{mode_offsets:?};"
    )
    .unwrap();
    for (mode, offset) in mode_offsets {
        writeln!(
            private_rs,
            "pub const FD_TABLE_OFFSET_{mode}: u16 = {offset};"
        )
        .unwrap();
        writeln!(semantic_tsv, "ROOT\t{mode}\t{offset}").unwrap();
    }
    let stats = format!(
        "Decode stats: Descs -- {} ({} bytes);  Trie -- {} bytes, {}; Mnems -- {} + {} bytes",
        descriptors.len(),
        8 * descriptors.len(),
        2 * table.len(),
        trie.stats(),
        string_table.len(),
        3 * intel_names.len(),
    );
    Ok(DecodeOutput {
        public_rs,
        private_rs,
        semantic_tsv,
        stats,
    })
}

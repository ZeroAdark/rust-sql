// SPDX-FileCopyrightText: 2024 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

use std::collections::BTreeMap;
use std::fmt::Write;

use crate::expression::{self, CType, Context, Function};
use crate::parser::{Desc, Fmt, MaskedVal};
use crate::{check_feature, Group};

pub const BITS_SUBGRP: u32 = 7;
pub const BITS_SUBTRIE: u32 = 9;
pub const GRP_START: u32 = 1 << (BITS_SUBTRIE - BITS_SUBGRP);
pub const MAX_SUBGRPS: usize = 1 << BITS_SUBGRP;
pub const MAX_SUBTRIES: usize = (1 << BITS_SUBTRIE) - 1;

#[derive(Clone, Debug)]
pub struct TrieDesc {
    pub fixed: MaskedVal,
    pub subgrp: u32,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct Trie {
    pub fmsk: u32,
    pub fval: u32,
    pub smsk_start: u32,
    pub smsk_len: u32,
    pub table: Vec<u32>,
    pub subtries: Vec<Trie>,
    pub comment: String,
}

impl Trie {
    pub fn table_size(&self) -> usize {
        self.table.len() + self.subtries.iter().map(Self::table_size).sum::<usize>()
    }

    pub fn codegen_c(&self, prefix: &str, coverage: bool) -> String {
        let names: Vec<String> = (1..=self.subtries.len())
            .map(|i| format!("{prefix}_{i:x}"))
            .collect();
        let mut out = self
            .subtries
            .iter()
            .zip(&names)
            .map(|(sub, name)| sub.codegen_c(name, coverage))
            .collect::<Vec<_>>()
            .join("\n");
        write!(
            out,
            "\n// {}\nstatic unsigned {prefix}(uint32_t inst) {{\n",
            self.comment
        )
        .unwrap();
        if self.fmsk != 0 {
            writeln!(
                out,
                "  if ((inst&{:#x}) != {:#x}) return 0;",
                self.fmsk, self.fval
            )
            .unwrap();
        }
        if self.subtries.is_empty() && self.table.iter().all(|v| *v == self.table[0]) {
            writeln!(out, "  return {:#x};\n}}", self.table[0]).unwrap();
            return out;
        }
        let index = format!(
            "(inst >> {}) & {:#x}",
            self.smsk_start,
            (1u32 << self.smsk_len) - 1
        );
        if coverage {
            write!(out, "  unsigned idx;\n  switch ({index}) {{\n").unwrap();
            for (i, value) in self.table.iter().enumerate() {
                writeln!(out, "    case {i:#x}: idx = {value:#x}; break;").unwrap();
            }
            out.push_str("  }\n");
        } else {
            out.push_str("  static const uint16_t table[] = {\n    ");
            out.push_str(
                &self
                    .table
                    .iter()
                    .map(|v| format!("{v:#x}"))
                    .collect::<Vec<_>>()
                    .join(","),
            );
            write!(out, " }};\n  unsigned idx = table[{index}];\n").unwrap();
        }
        if !self.subtries.is_empty() {
            out.push_str("  switch (idx) {\n");
            for (i, (sub, name)) in self.subtries.iter().zip(&names).enumerate() {
                writeln!(
                    out,
                    "    case {:#x}: return {name}(inst); // {}",
                    i + 1,
                    sub.comment
                )
                .unwrap();
            }
            out.push_str("  }\n");
        }
        out.push_str("  return idx;\n}\n");
        out
    }

    pub fn codegen_rust(&self, prefix: &str, coverage: bool) -> String {
        let names: Vec<String> = (1..=self.subtries.len())
            .map(|i| format!("{prefix}_{i:x}"))
            .collect();
        let mut out = String::new();
        for (sub, name) in self.subtries.iter().zip(&names) {
            out.push_str(&sub.codegen_rust(name, coverage));
        }
        writeln!(out, "pub fn {prefix}(inst: u32) -> u32 {{").unwrap();
        if self.fmsk != 0 {
            writeln!(
                out,
                "    if inst & {:#x} != {:#x} {{ return 0; }}",
                self.fmsk, self.fval
            )
            .unwrap();
        }
        if self.subtries.is_empty() && self.table.iter().all(|v| *v == self.table[0]) {
            writeln!(out, "    {:#x}\n}}", self.table[0]).unwrap();
            return out;
        }
        let index = format!(
            "(inst >> {}) & {:#x}",
            self.smsk_start,
            (1u32 << self.smsk_len) - 1
        );
        if coverage {
            writeln!(out, "    let idx: u32 = match {index} {{").unwrap();
            for (i, value) in self.table.iter().enumerate() {
                writeln!(out, "        {i:#x} => {value:#x},").unwrap();
            }
            out.push_str("        _ => unreachable!(),\n    };\n");
        } else {
            write!(out, "    const TABLE: [u16; {}] = [", self.table.len()).unwrap();
            out.push_str(
                &self
                    .table
                    .iter()
                    .map(|v| format!("{v:#x}"))
                    .collect::<Vec<_>>()
                    .join(","),
            );
            write!(out, "];\n    let idx = TABLE[({index}) as usize] as u32;\n").unwrap();
        }
        if !self.subtries.is_empty() {
            out.push_str("    match idx {\n");
            for (i, name) in names.iter().enumerate() {
                writeln!(out, "        {:#x} => {name}(inst),", i + 1).unwrap();
            }
            out.push_str("        _ => idx,\n    }\n");
        } else {
            out.push_str("    idx\n");
        }
        out.push_str("}\n");
        out
    }

    pub fn classify(&self, inst: u32) -> u32 {
        if inst & self.fmsk != self.fval {
            return 0;
        }
        let idx = ((inst >> self.smsk_start) & ((1u32 << self.smsk_len) - 1)) as usize;
        let value = self.table[idx];
        if value != 0 && value as usize <= self.subtries.len() {
            self.subtries[value as usize - 1].classify(inst)
        } else {
            value
        }
    }
}

pub fn make_table(descs: &[TrieDesc], keys: &[usize], prev_msk: u32) -> Result<Trie, String> {
    let mut counts = [[0usize; 2]; 32];
    let mut key_mask = u32::MAX;
    for &key in keys {
        let fixed = descs
            .get(key)
            .ok_or_else(|| format!("trie key {key} out of bounds"))?
            .fixed;
        key_mask &= fixed.mask;
        for (i, count) in counts.iter_mut().enumerate() {
            if fixed.mask & !prev_msk & (1u32 << i) != 0 {
                count[usize::from(fixed.val & (1u32 << i) != 0)] += 1;
            }
        }
    }
    let mut fmsk = 0;
    let mut fval = 0;
    for (i, count) in counts.iter().enumerate() {
        if (count[0] == 0 || count[1] == 0) && count[0] + count[1] != 0 {
            fmsk |= 1u32 << i;
        }
        if count[0] == 0 {
            fval |= 1u32 << i;
        }
    }
    fmsk &= key_mask;
    fval &= fmsk;
    let comment = if prev_msk == 0 {
        "*".to_owned()
    } else {
        keys.iter()
            .map(|&k| descs[k].name.as_str())
            .collect::<Vec<_>>()
            .join(",")
    };
    let Some(top) = counts
        .iter()
        .rposition(|count| count[0] != 0 && count[1] != 0)
    else {
        if keys.len() != 1 {
            return Err(format!("redundant opcodes: {keys:?} ({comment})"));
        }
        return Ok(Trie {
            fmsk,
            fval,
            smsk_start: 0,
            smsk_len: 0,
            table: vec![descs[keys[0]].subgrp],
            subtries: vec![],
            comment,
        });
    };
    let end = top + 1;
    let len = counts[..end]
        .iter()
        .rev()
        .take_while(|count| count[0] != 0 && count[1] != 0)
        .count()
        .min(12);
    let start = end - len;
    let smsk = ((1u32 << len) - 1) << start;
    let seen_msk = prev_msk | fmsk | smsk;
    let mut table_keys: Vec<Vec<usize>> = vec![vec![]; 1 << len];
    for &key in keys {
        let fixed = descs[key].fixed;
        let mask = (fixed.mask & smsk) >> start;
        let val = (fixed.val & smsk) >> start;
        let mut index = mask;
        while index < 1 << len {
            table_keys[((index & !mask) | val) as usize].push(key);
            index = (index + 1) | mask;
        }
    }
    let mut table = vec![0; 1 << len];
    let mut subtries = vec![];
    let mut subtrie_map: BTreeMap<Vec<usize>, u32> = BTreeMap::new();
    for (i, keys) in table_keys.into_iter().enumerate() {
        if keys.is_empty() {
            continue;
        }
        if keys.len() == 1 && descs[keys[0]].fixed.mask & !seen_msk == 0 {
            table[i] = descs[keys[0]].subgrp;
        } else if let Some(value) = subtrie_map.get(&keys) {
            table[i] = *value;
        } else {
            let sub = make_table(descs, &keys, seen_msk)?;
            let value = if sub.fmsk == 0 && sub.table.iter().all(|v| *v == sub.table[0]) {
                sub.table[0]
            } else {
                subtries.push(sub);
                subtries.len() as u32
            };
            subtrie_map.insert(keys, value);
            table[i] = value;
        }
    }
    if subtries.len() > MAX_SUBTRIES {
        return Err(format!(
            "too many subtries: {} > {MAX_SUBTRIES}",
            subtries.len()
        ));
    }
    Ok(Trie {
        fmsk,
        fval,
        smsk_start: start as u32,
        smsk_len: len as u32,
        table,
        subtries,
        comment,
    })
}

pub fn superstring(strings: &[String]) -> String {
    let mut ordered: Vec<&str> = strings.iter().map(String::as_str).collect();
    ordered.sort_by_key(|s| std::cmp::Reverse(s.chars().count()));
    let mut live: Vec<&str> = vec![];
    for text in ordered {
        if !live.iter().any(|entry| entry.contains(text)) {
            live.push(text);
        }
    }
    live.sort_unstable();
    let mut merged = String::new();
    while !live.is_empty() {
        let boundaries: Vec<usize> = merged.char_indices().map(|(i, _)| i).collect();
        let mut found = None;
        for size in (1..=16.min(boundaries.len())).rev() {
            let suffix = &merged[boundaries[boundaries.len() - size]..];
            let index = live.partition_point(|text| *text < suffix);
            if index < live.len() && live[index].starts_with(suffix) {
                found = Some((index, suffix.len()));
                break;
            }
        }
        if let Some((index, skip)) = found {
            merged.push_str(&live.remove(index)[skip..]);
        } else {
            merged.push_str(live.pop().unwrap());
        }
    }
    merged
}

#[derive(Clone, Debug)]
struct OperandDecoder {
    c: String,
    rust: String,
    names: Vec<String>,
}

#[derive(Clone, Debug)]
struct GroupDecoder {
    name: String,
    bodies: Vec<OperandDecoder>,
}

#[derive(Clone, Debug, Default)]
pub struct Decoder {
    pub mnems: Vec<(String, u32)>,
    pub grpnums: Vec<(String, u32)>,
    pub trie_descs: Vec<TrieDesc>,
    pub decstr: Vec<(u32, String)>,
    decoders: Vec<GroupDecoder>,
}

#[derive(Clone, Debug, Default)]
pub struct DecoderOutput {
    pub public: String,
    pub private: String,
    pub manifest: String,
    pub public_c: String,
    pub private_c: Vec<(String, String)>,
}

#[derive(Clone, Copy)]
struct Helper {
    name: &'static str,
    arguments: &'static [CType],
    state: bool,
    result: CType,
}

const U8: CType = CType::Unsigned(8);
const U16: CType = CType::Unsigned(16);
const U32: CType = CType::Unsigned(32);
const U64: CType = CType::Unsigned(64);
const I16: CType = CType::Signed(16);
const I64: CType = CType::Signed(64);
const BOOL: CType = CType::Bool;
const OP: CType = CType::Opaque;

const HELPERS: &[Helper] = &[
    Helper {
        name: "sext",
        arguments: &[U64, U32],
        state: false,
        result: U64,
    },
    Helper {
        name: "clz",
        arguments: &[U32, U32],
        state: false,
        result: U32,
    },
    Helper {
        name: "ctz",
        arguments: &[U32],
        state: false,
        result: U32,
    },
    Helper {
        name: "OPreggp",
        arguments: &[U32, BOOL],
        state: false,
        result: OP,
    },
    Helper {
        name: "OPreggpinc",
        arguments: &[U32],
        state: false,
        result: OP,
    },
    Helper {
        name: "OPreggpsp",
        arguments: &[U32, BOOL],
        state: false,
        result: OP,
    },
    Helper {
        name: "OPreggpmaysp",
        arguments: &[BOOL, U32, BOOL],
        state: false,
        result: OP,
    },
    Helper {
        name: "OPreggpprf",
        arguments: &[BOOL, U32, BOOL],
        state: false,
        result: OP,
    },
    Helper {
        name: "OPreggpext",
        arguments: &[U32, BOOL, U32, U32],
        state: false,
        result: OP,
    },
    Helper {
        name: "OPregfp",
        arguments: &[U32, U32],
        state: false,
        result: OP,
    },
    Helper {
        name: "OPregvec",
        arguments: &[U32, U32, BOOL],
        state: false,
        result: OP,
    },
    Helper {
        name: "OPregvidx",
        arguments: &[U32, U32, U32],
        state: false,
        result: OP,
    },
    Helper {
        name: "OPregvtbl",
        arguments: &[U32, U32, BOOL, U32],
        state: false,
        result: OP,
    },
    Helper {
        name: "OPregvtblidx",
        arguments: &[U32, U32, U32, U32],
        state: false,
        result: OP,
    },
    Helper {
        name: "OPmemuoff",
        arguments: &[U32, U16],
        state: false,
        result: OP,
    },
    Helper {
        name: "OPmemsoff",
        arguments: &[U32, I16],
        state: false,
        result: OP,
    },
    Helper {
        name: "OPmemsoffpre",
        arguments: &[U32, I16],
        state: false,
        result: OP,
    },
    Helper {
        name: "OPmemsoffpost",
        arguments: &[U32, I16],
        state: false,
        result: OP,
    },
    Helper {
        name: "OPmemreg",
        arguments: &[U32, U32, U32, BOOL, U32],
        state: false,
        result: OP,
    },
    Helper {
        name: "OPmemregsimdpost",
        arguments: &[U32, U32, U32],
        state: false,
        result: OP,
    },
    Helper {
        name: "OPmeminc",
        arguments: &[U32],
        state: false,
        result: OP,
    },
    Helper {
        name: "OPimmsmall",
        arguments: &[U32],
        state: false,
        result: OP,
    },
    Helper {
        name: "OPsimm",
        arguments: &[I16],
        state: false,
        result: OP,
    },
    Helper {
        name: "OPuimm",
        arguments: &[U16],
        state: false,
        result: OP,
    },
    Helper {
        name: "OPuimmshift",
        arguments: &[U16, BOOL, U32],
        state: false,
        result: OP,
    },
    Helper {
        name: "OPreladdr",
        arguments: &[I64],
        state: true,
        result: OP,
    },
    Helper {
        name: "OPimmlogical",
        arguments: &[U32, U32, U32, U32],
        state: true,
        result: OP,
    },
    Helper {
        name: "OPimmsimdmask",
        arguments: &[U8],
        state: true,
        result: OP,
    },
    Helper {
        name: "OPimmfloatzero",
        arguments: &[],
        state: true,
        result: OP,
    },
    Helper {
        name: "OPimmfloat",
        arguments: &[U8],
        state: true,
        result: OP,
    },
    Helper {
        name: "OPsysreg",
        arguments: &[U32],
        state: false,
        result: OP,
    },
    Helper {
        name: "OPcond",
        arguments: &[U32],
        state: false,
        result: OP,
    },
];

fn helper_type(ty: CType) -> &'static str {
    match ty {
        BOOL => "bool",
        U8 => "u8",
        U16 => "u16",
        U32 => "u32",
        U64 => "u64",
        I16 => "i16",
        I64 => "i64",
        OP => "Self::Op",
        _ => unreachable!(),
    }
}

fn decoder_context(desc: &Desc, referenced: &[&str]) -> Context {
    let mut context = Context::default();
    context.variables.insert("ddi".into(), CType::Opaque);
    context.variables.insert("inst".into(), U32);
    context.variables.insert("mnem".into(), U32);
    for entry in desc {
        if let Some(name) = entry
            .name
            .as_ref()
            .filter(|name| referenced.contains(&name.as_str()))
        {
            context.variables.insert(name.clone(), CType::Unsigned(64));
        }
    }
    for helper in HELPERS {
        let mut arguments = Vec::new();
        if helper.state {
            arguments.push(CType::Opaque);
        }
        arguments.extend_from_slice(helper.arguments);
        context.functions.insert(
            helper.name.into(),
            Function {
                rust_name: format!("H::{}", helper.name),
                arguments,
                result: helper.result,
            },
        );
    }
    context
}

fn operand_strings(text: &str) -> Result<Vec<String>, String> {
    let without_markers = text.replace('@', "");
    let mut operands: Vec<String> = Vec::new();
    let mut depth = 0i32;
    for word in without_markers
        .split(crate::python_whitespace)
        .filter(|word| !word.is_empty())
    {
        if depth != 0 {
            let last = operands
                .last_mut()
                .ok_or_else(|| "invalid operand parentheses".to_owned())?;
            last.push(' ');
            last.push_str(word);
        } else {
            operands.push(word.to_owned());
        }
        depth += word.bytes().filter(|b| *b == b'(').count() as i32;
        depth -= word.bytes().filter(|b| *b == b')').count() as i32;
        if depth < 0 {
            return Err("invalid operand parentheses".into());
        }
    }
    if depth != 0 {
        return Err("unclosed operand parentheses".into());
    }
    for operand in &mut operands {
        operand.truncate(operand.trim_end_matches(',').len());
    }
    Ok(operands)
}

pub fn generate_opdecoder(text: &str, desc: &Desc) -> Result<(String, String), String> {
    let names: Vec<&str> = text
        .split('@')
        .skip(1)
        .map(|s| {
            s.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .next()
                .unwrap_or("")
        })
        .collect();
    let mut c = "{".to_owned();
    let mut rust = "{\n".to_owned();
    for entry in desc {
        let Some(name) = entry.name.as_deref().filter(|name| names.contains(name)) else {
            continue;
        };
        let mask = u32::MAX >> (32 - entry.size);
        let value = if entry.fixed.mask == mask {
            entry.fixed.val.to_string()
        } else {
            format!("inst>>{}&{mask:#x}", entry.idx)
        };
        let rust_value = if entry.fixed.mask == mask {
            format!("{}u64", entry.fixed.val)
        } else {
            format!("((inst >> {}) & {mask:#x}) as u64", entry.idx)
        };
        if entry.flags == "s" {
            write!(c, "uint64_t {name} = sext({value}, {}); ", entry.size).unwrap();
            writeln!(
                rust,
                "    let {}: u64 = H::sext({rust_value}, {}u32);",
                expression::identifier(name),
                entry.size
            )
            .unwrap();
        } else {
            write!(c, "uint64_t {name} = {value}; ").unwrap();
            writeln!(
                rust,
                "    let {}: u64 = {rust_value};",
                expression::identifier(name)
            )
            .unwrap();
        }
    }
    let context = decoder_context(desc, &names);
    for (i, operand) in operand_strings(text)?.iter().enumerate() {
        if i >= 5 {
            return Err("decoder exceeds five operands".into());
        }
        write!(c, "ddi->ops[{i}] = {operand}; ").unwrap();
        let rendered = expression::render(operand, &context)?;
        if rendered.ty != CType::Opaque {
            return Err(format!("operand does not produce an operand: {operand}"));
        }
        writeln!(
            rust,
            "    let operand = {};\n    H::set_operand(ddi, {i}, operand);",
            rendered.code
        )
        .unwrap();
    }
    c.push_str("break;}");
    rust.push_str("}");
    Ok((c, rust))
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_group(&mut self, group: &Group, features: &[(String, bool)]) -> Result<(), String> {
        if self.grpnums.iter().any(|(name, _)| name == &group.name) {
            return Err(format!("redundant group {}", group.name));
        }
        let group_number =
            u32::try_from(self.grpnums.len()).map_err(|_| "too many decoder groups")? + GRP_START;
        if group_number >= 1 << (16 - BITS_SUBGRP) {
            return Err("decoder group number exceeds sixteen bits".into());
        }
        let Some(decode) = group.get("decode") else {
            self.grpnums.push((group.name.clone(), group_number));
            return Ok(());
        };
        let decodefmt = Fmt::parse(decode, &group.desc)?;
        let featfmt = Fmt::parse(group.get("feat").unwrap_or(""), &group.desc)?;
        let mnemfmt = Fmt::parse(
            group
                .get("mnem")
                .ok_or_else(|| format!("group {} without mnem", group.name))?,
            &group.desc,
        )?;
        let expansions = mnemfmt.expansions()?;
        if expansions.len() > MAX_SUBGRPS {
            return Err(format!(
                "group {} exceeds {MAX_SUBGRPS} mnemonics",
                group.name
            ));
        }
        let mut mnems = Vec::new();
        let mut trie_descs = Vec::new();
        let mut decstr = Vec::new();
        let mut bodies: Vec<OperandDecoder> = Vec::new();
        for (i, (name, masks)) in expansions.into_iter().enumerate() {
            if self
                .mnems
                .iter()
                .chain(&mnems)
                .any(|(prior, _)| prior == &name)
            {
                return Err(format!("redundant mnem {name}"));
            }
            let number = (group_number << BITS_SUBGRP) + i as u32;
            mnems.push((name.clone(), number));
            let feature = featfmt
                .eval(&masks)?
                .ok_or_else(|| format!("missing feature expansion for {name}"))?;
            if !check_feature(features, &feature)? {
                continue;
            }
            for (j, &fixed) in masks.iter().enumerate() {
                trie_descs.push(TrieDesc {
                    fixed,
                    subgrp: number,
                    name: format!("{}-{name}#{j}", group.name),
                });
            }
            let decoded = decodefmt
                .eval(&masks)?
                .ok_or_else(|| format!("missing decode expansion for {name}"))?;
            let (decode_name, operand_text) = decoded.split_once(' ').unwrap_or((&decoded, ""));
            decstr.push((number, decode_name.replace('_', " ")));
            let (c, rust) = generate_opdecoder(operand_text, &group.desc)?;
            if let Some(body) = bodies.iter_mut().find(|body| body.c == c) {
                body.names.push(name);
            } else {
                bodies.push(OperandDecoder {
                    c,
                    rust,
                    names: vec![name],
                });
            }
        }
        self.grpnums.push((group.name.clone(), group_number));
        self.mnems.extend(mnems);
        self.trie_descs.extend(trie_descs);
        self.decstr.extend(decstr);
        self.decoders.push(GroupDecoder {
            name: group.name.clone(),
            bodies,
        });
        Ok(())
    }

    pub fn generate(&self, coverage: bool) -> Result<DecoderOutput, String> {
        let trie = make_table(
            &self.trie_descs,
            &(0..self.trie_descs.len()).collect::<Vec<_>>(),
            0,
        )?;
        let mut out = DecoderOutput::default();
        let mut mnems_c = String::new();
        let mut groups_c = String::new();
        out.public
            .push_str("pub const DA64I_UNKNOWN: u32 = 0;\npub const DA64G_UNKNOWN: u32 = 0;\n");
        for (name, value) in &self.mnems {
            writeln!(mnems_c, "DA64I_{name}={value:#x},").unwrap();
            writeln!(out.public, "pub const DA64I_{name}: u32 = {value:#x};").unwrap();
        }
        for (name, value) in &self.grpnums {
            writeln!(groups_c, "DA64G_{name}={value:#x},").unwrap();
            writeln!(out.public, "pub const DA64G_{name}: u32 = {value:#x};").unwrap();
            writeln!(out.manifest, "group\t{name}\t{value}").unwrap();
        }
        for (name, value) in &self.mnems {
            writeln!(out.manifest, "mnemonic\t{name}\t{value}").unwrap();
        }
        for entry in &self.trie_descs {
            writeln!(
                out.manifest,
                "trie_desc\t{:x}\t{:x}\t{}\t{}",
                entry.fixed.val, entry.fixed.mask, entry.subgrp, entry.name
            )
            .unwrap();
        }
        out.public_c = format!(
            "\nenum Da64InstKind {{\n  DA64I_UNKNOWN=0,\n  {mnems_c}\n}};\nenum Da64InstGroup {{\n  DA64G_UNKNOWN=0,\n  {groups_c}\n}};\n#define DA64_GROUP(mnem) ((mnem) >> {BITS_SUBGRP})\n"
        );
        writeln!(
            out.public,
            "pub const fn da64_group(mnem: u32) -> u32 {{ mnem >> {BITS_SUBGRP} }}"
        )
        .unwrap();
        out.private
            .push_str(&trie.codegen_rust("da64_classify_impl", coverage));
        let strings: Vec<String> = self.decstr.iter().map(|(_, s)| s.clone()).collect();
        let packed = superstring(&strings);
        let mut entries = vec![
            0u16;
            self.decstr
                .iter()
                .map(|(value, _)| *value as usize + 1)
                .max()
                .unwrap_or(0)
        ];
        let mut table_c = String::new();
        for (value, text) in &self.decstr {
            let byte_offset = packed
                .find(text)
                .ok_or_else(|| "mnemonic missing from superstring".to_owned())?;
            let offset = packed[..byte_offset].chars().count();
            let length = text.chars().count();
            if offset >= 4096 || length >= 16 {
                return Err(format!(
                    "packed decoder mnemonic exceeds field bounds: {text}"
                ));
            }
            let index = (length << 12) | offset;
            entries[*value as usize] = index as u16;
            writeln!(table_c, "[{value}] = {index:#x},").unwrap();
            writeln!(out.manifest, "decstr\t{value}\t{text}").unwrap();
        }
        writeln!(out.private, "pub const DA64_DECSTR: &str = {packed:?};").unwrap();
        out.private
            .push_str("pub const DA64_DECSTRTAB: &[u16] = &[\n");
        for chunk in entries.chunks(32) {
            out.private.push_str("    ");
            for value in chunk {
                write!(out.private, "{value:#x},").unwrap();
            }
            out.private.push('\n');
        }
        out.private.push_str("];\n");
        let (decoder_c, decoder_rust) = self.codegen_decoders();
        out.private.push_str(&decoder_rust);
        out.private_c = vec![
            (
                "DA64_CLASSIFIER".into(),
                trie.codegen_c("da64_classify_impl", coverage),
            ),
            ("DA64_DECSTR".into(), format!("\"{packed}\"")),
            ("DA64_DECSTRTAB".into(), table_c),
            ("DA64_DECODER".into(), decoder_c),
        ];
        for helper in HELPERS {
            let args = helper
                .arguments
                .iter()
                .map(|ty| helper_type(*ty))
                .collect::<Vec<_>>()
                .join(",");
            writeln!(
                out.manifest,
                "decoder_helper\t{}\t{args}\t{}\t{}",
                helper.name,
                u8::from(helper.state),
                helper_type(helper.result)
            )
            .unwrap();
        }
        Ok(out)
    }

    fn codegen_decoders(&self) -> (String, String) {
        let mut c = String::new();
        let mut rust = "#[allow(non_snake_case)]\npub trait DecoderHelpers {\n    type State;\n    type Op;\n    fn set_operand(ddi: &mut Self::State, index: usize, operand: Self::Op);\n".to_owned();
        for helper in HELPERS {
            write!(rust, "    fn {}(", helper.name).unwrap();
            if helper.state {
                rust.push_str("ddi: &mut Self::State");
            }
            for (arg, ty) in helper.arguments.iter().enumerate() {
                if helper.state || arg != 0 {
                    rust.push_str(", ");
                }
                write!(rust, "arg{arg}: {}", helper_type(*ty)).unwrap();
            }
            writeln!(rust, ") -> {};", helper_type(helper.result)).unwrap();
        }
        rust.push_str("}\n#[allow(non_snake_case)]\npub fn da64_decode_operands<H: DecoderHelpers>(inst: u32, mnem: u32, ddi: &mut H::State) {\n    match da64_group(mnem) {\n");
        for group in &self.decoders {
            write!(c, "case DA64G_{}: ", group.name).unwrap();
            write!(rust, "        DA64G_{} => ", group.name).unwrap();
            if group.bodies.len() == 1 {
                c.push_str(&group.bodies[0].c);
                c.push('\n');
                rust.push_str(&group.bodies[0].rust);
                rust.push_str(",\n");
            } else {
                c.push_str("switch (mnem) { default: __builtin_unreachable();\n");
                rust.push_str("match mnem {\n");
                for body in &group.bodies {
                    c.push_str("  ");
                    c.push_str(
                        &body
                            .names
                            .iter()
                            .map(|name| format!("case DA64I_{name}:"))
                            .collect::<Vec<_>>()
                            .join(" "),
                    );
                    write!(c, "\n    {}\n", body.c).unwrap();
                    rust.push_str("            ");
                    rust.push_str(
                        &body
                            .names
                            .iter()
                            .map(|name| format!("DA64I_{name}"))
                            .collect::<Vec<_>>()
                            .join(" | "),
                    );
                    writeln!(rust, " => {},", body.rust).unwrap();
                }
                c.push_str("} break;\n");
                rust.push_str("            _ => unreachable!(\"mnemonic outside enabled decoder forms\"),\n        },\n");
            }
        }
        rust.push_str("        _ => {},\n    }\n}\n");
        (c, rust)
    }
}

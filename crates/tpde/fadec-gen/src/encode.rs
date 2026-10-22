// SPDX-FileCopyrightText: 2018 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

use std::collections::BTreeMap;
use std::fmt::Write;

use crate::Result;
use crate::model::{Entry, InstrDesc, Opcode, encoding, op_order};

#[derive(Clone, Debug)]
pub struct EncodeVariant {
    pub opcode: Opcode,
    pub desc: InstrDesc,
    pub evexbcst: bool,
    pub evexmask: u8,
    pub evexsae: u8,
    pub evexdisp8scale: i32,
    pub downgrade: u8,
    pub flexcc: bool,
}

#[derive(Clone, Debug)]
pub struct Mnemonic {
    pub name: String,
    pub opsize: i32,
    pub ots: String,
    pub variants: Vec<EncodeVariant>,
}

#[derive(Clone, Debug)]
pub struct EncodeOutput {
    pub public_rs: String,
    pub private_rs: String,
    pub semantic_tsv: String,
}

impl EncodeVariant {
    pub fn new(opcode: Opcode, desc: InstrDesc) -> Self {
        Self {
            opcode,
            desc,
            evexbcst: false,
            evexmask: 0,
            evexsae: 0,
            evexdisp8scale: 0,
            downgrade: 0,
            flexcc: false,
        }
    }
}

fn add_variant(
    mnemonics: &mut Vec<Mnemonic>,
    indices: &mut BTreeMap<(String, i32, String), usize>,
    name: String,
    opsize: i32,
    ots: &str,
    variant: EncodeVariant,
) {
    let key = (name.clone(), opsize, ots.to_owned());
    if let Some(&index) = indices.get(&key) {
        mnemonics[index].variants.push(variant);
    } else {
        indices.insert(key, mnemonics.len());
        mnemonics.push(Mnemonic {
            name,
            opsize,
            ots: ots.to_owned(),
            variants: vec![variant],
        });
    }
}

struct Form<'a> {
    opsize: i32,
    vecsize: i32,
    prefix: (&'a str, &'a str),
    ots: &'a str,
    evexmask: u8,
    evexsae: u8,
    prepend_opsize: bool,
    prepend_vecsize: bool,
    separate_opsize: bool,
}

fn expand_form(
    opcode: &Opcode,
    desc: &InstrDesc,
    mnem_name: &str,
    form: Form<'_>,
) -> Result<Option<(String, i32, EncodeVariant)>> {
    let Form {
        mut opsize,
        vecsize,
        prefix,
        ots,
        evexmask,
        evexsae,
        mut prepend_opsize,
        prepend_vecsize,
        separate_opsize,
    } = form;
    let has_memory = ots.contains('m') || ots.contains('b');
    if prefix.1 == "LOCK" && !ots.starts_with('m')
        || evexmask == 2 && !ots.starts_with('r')
        || evexsae != 0 && (!matches!(vecsize, 0 | 512) || has_memory)
    {
        return Ok(None);
    }

    let flags = encoding(&desc.encoding)?;
    let mut spec_opcode = opcode.clone();
    if !prefix.1.is_empty() {
        spec_opcode.prefix = Some(prefix.1.to_owned());
    }
    if opsize == 64 && !desc.flags.contains("D64") && !desc.flags.contains("F64") {
        spec_opcode.rexw = Some("1".to_owned());
    }
    if let Some(vexl) = match vecsize {
        512 => Some("2"),
        256 => Some("1"),
        128 => Some("0"),
        _ => None,
    } {
        spec_opcode.vexl = Some(vexl.to_owned());
    }
    if spec_opcode.vexl.as_deref() == Some("IG") {
        spec_opcode.vexl = Some("0".to_owned());
    }
    if flags.modrm_idx != 0 {
        spec_opcode.modrm.0 = Some(if has_memory { 'm' } else { 'r' });
    }
    if (flags.modrm != 0
        || opcode.modrm.0.is_some() && opcode.modrm.1.is_some() && opcode.modrm.2.is_some())
        && !matches!(spec_opcode.modrm.0, Some('r' | 'm'))
    {
        return Err(format!("unspecialized ModRM for {}", desc.mnemonic));
    }

    let evexbcst = ots.contains('b');
    let mut evexdisp8scale = 0;
    if spec_opcode.vex == 2 && has_memory {
        if !evexbcst {
            let op = desc
                .operands
                .get((flags.modrm_idx ^ 3) as usize)
                .ok_or_else(|| format!("missing memory operand in {}", desc.mnemonic))?;
            let size = op.abssize(Some(opsize / 8), Some(vecsize / 8))?;
            evexdisp8scale = if size == 0 {
                -1
            } else {
                31 - size.unsigned_abs().leading_zeros() as i32
            };
        } else if desc.flags.contains("BCST16") {
            evexdisp8scale = 1;
        } else {
            evexdisp8scale = if spec_opcode.rexw.as_deref() == Some("1") {
                3
            } else {
                2
            };
        }
    }

    let mut name = format!("{}{mnem_name}", prefix.0);
    if matches!(desc.mnemonic.as_str(), "EVX_MOV_G2X" | "EVX_MOV_X2G") {
        name.insert(name.len() - 4, if opsize == 64 { 'Q' } else { 'D' });
        prepend_opsize = false;
        opsize = 0;
    }
    if matches!(name.as_str(), "VMOVD_G2X" | "VMOVD_X2G") && has_memory {
        name = name.replace("_G2X", "").replace("_X2G", "");
    }
    let suffix_operand = match desc.mnemonic.as_str() {
        "EVX_PEXTR" => Some(0),
        "EVX_PBROADCAST" => Some(1),
        "EVX_PINSR" => Some(2),
        _ => None,
    };
    if let Some(index) = suffix_operand {
        let op = desc
            .operands
            .get(index)
            .ok_or_else(|| format!("missing suffix operand in {}", desc.mnemonic))?;
        let size = op.abssize(Some(opsize / 8), Some(vecsize / 8))?;
        name.push(
            *b" BW D   Q"
                .get(size as usize)
                .ok_or_else(|| format!("invalid suffix size {size} in {}", desc.mnemonic))?
                as char,
        );
        if desc.mnemonic == "EVX_PBROADCAST" {
            name.push_str("_GP");
        }
        prepend_opsize = false;
        opsize = 0;
    }
    if prepend_opsize && !(desc.flags.contains("D64") && opsize == 64) {
        append_size(&mut name, opsize);
    }
    if prepend_vecsize {
        append_size(&mut name, vecsize);
    }
    for (ot, op) in ots.chars().zip(&desc.operands) {
        if ot != 'o' {
            name.push(ot);
        }
        if separate_opsize {
            write!(
                name,
                "{}",
                op.abssize(Some(opsize / 8), Some(vecsize / 8))? * 8
            )
            .unwrap();
        }
    }
    if !desc.flags.contains("VSIB") {
        name.push_str(["", "_mask", "_maskz"][evexmask as usize]);
    }
    name.push_str(["", "_sae", "_er"][evexsae as usize]);
    let mut variant = EncodeVariant::new(spec_opcode, desc.clone());
    variant.evexbcst = evexbcst;
    variant.evexmask = evexmask;
    variant.evexsae = evexsae;
    variant.evexdisp8scale = evexdisp8scale;
    Ok(Some((name, opsize, variant)))
}

fn append_size(name: &mut String, size: i32) {
    if name.as_bytes().last().is_some_and(u8::is_ascii_digit) {
        name.push('_');
    }
    write!(name, "{size}").unwrap();
}

pub fn encode_mnems(entries: &[Entry]) -> Result<Vec<Mnemonic>> {
    let mut mnemonics = Vec::new();
    let mut indices = BTreeMap::new();
    add_variant(
        &mut mnemonics,
        &mut indices,
        "PAUSE".to_owned(),
        0,
        "",
        EncodeVariant::new(Opcode::parse("F3.90")?, InstrDesc::parse("NP - - - - NOP")?),
    );
    for entry in entries {
        let (opcode, desc) = (&entry.opcode, &entry.desc);
        if desc.flags.contains("I64") || desc.mnemonic.starts_with("RESERVED_") {
            continue;
        }
        let mnem_name = match desc.mnemonic.as_str() {
            "MOVABS" => "MOV",
            "XCHG_NOP" => "XCHG",
            name => name,
        }
        .replace("EVX_", "V");
        let mut opsizes = vec![0];
        let mut vecsizes = vec![0];
        let mut prepend_opsize = false;
        let mut prepend_vecsize = false;
        let separate_opsize = desc.flags.contains("ENC_SEPSZ");
        let dynsizes = if desc.flags.contains("ENC_NOSZ") {
            Default::default()
        } else {
            desc.dynsizes()?
        };
        if dynsizes.contains(&-1) {
            if opcode.rexw.is_some() {
                return Err(format!("unexpected REXW specifier {}", desc.mnemonic));
            }
            opsizes = if desc.flags.contains("SZ8") {
                vec![8]
            } else {
                vec![16, 32, 64]
            };
            if matches!(opcode.prefix.as_deref(), Some("NP" | "66" | "F2" | "F3"))
                && !desc.flags.contains("U66")
                || desc.flags.contains("I66")
            {
                opsizes.retain(|&size| size != 16);
            }
            if desc.flags.contains("D64") {
                opsizes.retain(|&size| size != 32);
            }
            prepend_opsize = !separate_opsize;
            if desc.flags.contains("F64") {
                opsizes = vec![64];
                prepend_opsize = false;
            }
        } else if !dynsizes.is_empty() && opcode.vex != 0 && opcode.vexl.as_deref() != Some("IG") {
            vecsizes = if opcode.vex == 2 {
                vec![128, 256, 512]
            } else {
                vec![128, 256]
            };
            if let Some(vexl) = opcode.vexl.as_deref().filter(|s| !s.is_empty()) {
                vecsizes = vexl
                    .chars()
                    .map(|c| {
                        c.to_digit(10)
                            .filter(|&n| n <= 2)
                            .map(|n| 128 << n)
                            .ok_or_else(|| format!("invalid VEX.L {vexl}"))
                    })
                    .collect::<Result<Vec<_>>>()?;
                vecsizes.sort_unstable();
                vecsizes.dedup();
            }
            prepend_vecsize = !separate_opsize;
        }
        let flags = encoding(&desc.encoding)?;
        let order = op_order(&desc.encoding)?;
        let mut optypes = vec![String::new()];
        for (i, op) in desc.operands.iter().enumerate() {
            let reg = if op.kind() == "MASK" { 'k' } else { 'r' };
            let options = match order.get(i).copied() {
                Some("modrm") => {
                    let mut types = opcode
                        .modrm
                        .0
                        .map_or_else(|| "rm".to_owned(), |c| c.to_string())
                        .replace('r', &reg.to_string());
                    if opcode.extended
                        || matches!(
                            desc.mnemonic.as_str(),
                            "MOV_CR2G" | "MOV_DR2G" | "MOV_G2CR" | "MOV_G2DR"
                        )
                    {
                        types = reg.to_string();
                    }
                    if desc.flags.contains("BCST") {
                        types.push('b');
                    }
                    types
                }
                Some("modreg" | "vexreg") => reg.to_string(),
                Some(_) => (*b" iariioo"
                    .get(flags.imm_control as usize)
                    .ok_or_else(|| format!("invalid immediate control {}", flags.imm_control))?
                    as char)
                    .to_string(),
                None => return Err(format!("too many operands in {}", desc.mnemonic)),
            };
            let mut expanded = Vec::new();
            for base in &optypes {
                for option in options.chars() {
                    let mut value = base.clone();
                    value.push(option);
                    expanded.push(value);
                }
            }
            optypes = expanded;
        }
        let mut prefixes = vec![("", "")];
        if desc.flags.contains("LOCK") {
            prefixes.push(("LOCK_", "LOCK"));
        }
        if desc.flags.contains("ENC_REP") {
            prefixes.push(("REP_", "F3"));
        }
        if desc.flags.contains("ENC_REPCC") {
            prefixes.extend_from_slice(&[("REPNZ_", "F2"), ("REPZ_", "F3")]);
        }
        let mut evexmasks = vec![0];
        if desc.flags.contains("MASK") {
            if desc.flags.contains("VSIB") {
                evexmasks = vec![1];
            } else {
                evexmasks.push(1);
                if desc
                    .operands
                    .first()
                    .ok_or_else(|| format!("missing mask destination in {}", desc.mnemonic))?
                    .kind()
                    != "MASK"
                {
                    evexmasks.push(2);
                }
            }
        }
        let mut evexsaes = vec![0];
        if desc.flags.contains("SAE") {
            evexsaes.push(1);
        } else if desc.flags.contains("ER") {
            evexsaes.push(2);
        }

        for &opsize in &opsizes {
            for &vecsize in &vecsizes {
                for &prefix in &prefixes {
                    for ots in &optypes {
                        for &evexmask in &evexmasks {
                            for &evexsae in &evexsaes {
                                let form = Form {
                                    opsize,
                                    vecsize,
                                    prefix,
                                    ots,
                                    evexmask,
                                    evexsae,
                                    prepend_opsize,
                                    prepend_vecsize,
                                    separate_opsize,
                                };
                                let Some((name, size, variant)) =
                                    expand_form(opcode, desc, &mnem_name, form)?
                                else {
                                    continue;
                                };
                                add_variant(
                                    &mut mnemonics,
                                    &mut indices,
                                    name.clone(),
                                    size,
                                    ots,
                                    variant.clone(),
                                );
                                let altname = match name.as_str() {
                                    "C_EX16" => Some("CBW"),
                                    "C_EX32" => Some("CWDE"),
                                    "C_EX64" => Some("CDQE"),
                                    "C_SEP16" => Some("CWD"),
                                    "C_SEP32" => Some("CDQ"),
                                    "C_SEP64" => Some("CQO"),
                                    "CMPXCHGD32m" => Some("CMPXCHG8Bm"),
                                    "CMPXCHGD64m" => Some("CMPXCHG16Bm"),
                                    _ => None,
                                };
                                if let Some(altname) = altname {
                                    add_variant(
                                        &mut mnemonics,
                                        &mut indices,
                                        altname.to_owned(),
                                        size,
                                        ots,
                                        variant.clone(),
                                    );
                                }
                                if desc.flags.contains("ENC_CC_BEGIN") {
                                    let ccname = if let Some(at) = name.rfind('O') {
                                        format!("{}cc{}", &name[..at], &name[at + 1..])
                                    } else {
                                        name
                                    };
                                    let mut ccvariant = variant;
                                    ccvariant.flexcc = true;
                                    add_variant(
                                        &mut mnemonics,
                                        &mut indices,
                                        ccname,
                                        size,
                                        ots,
                                        ccvariant,
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    for mnemonic in &mut mnemonics {
        let mut dedup = BTreeMap::new();
        for (i, variant) in std::mem::take(&mut mnemonic.variants)
            .into_iter()
            .enumerate()
        {
            let priority = ["O", "OA", "AO", "AM", "MA", "IA", "OI"]
                .iter()
                .position(|&name| name == variant.desc.encoding)
                .unwrap_or(7);
            let unique = if variant.desc.encoding == "S" { i } else { 0 };
            let key = (
                variant.desc.imm_size(mnemonic.opsize / 8)?,
                variant.opcode.vex,
                priority,
                unique,
            );
            dedup.entry(key).or_insert(variant);
        }
        mnemonic.variants = dedup.into_values().collect();
        if mnemonic.variants.len() > 1 && mnemonic.variants.iter().any(|v| v.opcode.vex != 0) {
            if mnemonic.variants.len() != 2 {
                return Err(format!(
                    "VEX/EVEX mnemonic with more than two encodings {}",
                    mnemonic.name
                ));
            }
            let (no_evex, evex) = (&mnemonic.variants[0], &mnemonic.variants[1]);
            if no_evex.opcode.vex == 2 || evex.opcode.vex != 2 {
                return Err(format!(
                    "EVEX mnemonic not with non-EVEX pair {}",
                    mnemonic.name
                ));
            }
            if no_evex.opcode.prefix != evex.opcode.prefix
                || no_evex.opcode.escape != evex.opcode.escape
                || no_evex.opcode.opc != evex.opcode.opc
                || no_evex.opcode.modrm.1 != evex.opcode.modrm.1
                || no_evex.opcode.modrm.2 != evex.opcode.modrm.2
                || no_evex.opcode.vexl != evex.opcode.vexl
                || no_evex.desc.encoding != evex.desc.encoding
                || no_evex.desc.operands.len() != evex.desc.operands.len()
                || no_evex
                    .desc
                    .operands
                    .iter()
                    .zip(&evex.desc.operands)
                    .any(|(a, b)| a.kind() != b.kind() || a.size() != b.size())
            {
                return Err(format!("cannot downgrade EVEX? {}", mnemonic.name));
            }
            let rexw_flip = (no_evex.opcode.rexw.as_deref() == Some("1"))
                != (evex.opcode.rexw.as_deref() == Some("1"));
            let mut selected = mnemonic.variants.pop().unwrap();
            selected.downgrade = if rexw_flip { 2 } else { 1 };
            mnemonic.variants.clear();
            mnemonic.variants.push(selected);
        }
    }
    Ok(mnemonics)
}

fn encode_variant(variant: &EncodeVariant, opsize: i32, high_regs: u64, alt: usize) -> Result<u64> {
    let (opcode, desc) = (&variant.opcode, &variant.desc);
    let flags = encoding(&desc.encoding)?;
    let mut value = u64::from(opcode.opc);
    if let (Some(_), Some(reg), Some(rm)) = opcode.modrm {
        value |= 0xc000 | (u64::from(reg) << 11) | (u64::from(rm) << 8);
    } else if let Some(reg) = opcode.modrm.1 {
        value |= u64::from(reg) << 8;
    }
    if opcode.modrm == (Some('m'), None, Some(4)) {
        value |= 0x2000000000;
    }
    let prefix = opcode.prefix.as_deref();
    if opcode.vex == 0 {
        if opcode.escape >= 4 {
            return Err("legacy opcode escape exceeds three".to_owned());
        }
        value |= u64::from(opcode.escape) * 0x10000;
        if prefix == Some("66") || opsize == 16 {
            value |= 0x80000;
        }
        if prefix == Some("F2") {
            value |= 0x100000;
        }
        if prefix == Some("F3") {
            value |= 0x200000;
        }
    } else {
        if opcode.escape >= 8 {
            return Err("VEX opcode escape exceeds seven".to_owned());
        }
        value |= u64::from(opcode.escape) * 0x10000;
        if prefix == Some("66") || opsize == 16 {
            if matches!(prefix, Some("F2" | "F3")) {
                return Err("conflicting VEX mandatory prefix and operand size".to_owned());
            }
            value |= 0x100000;
        }
        if prefix == Some("F3") {
            value |= 0x200000;
        } else if prefix == Some("F2") {
            value |= 0x300000;
        }
    }
    if opcode.rexw.as_deref() == Some("1") {
        value |= 0x400000;
    }
    if prefix == Some("LOCK") {
        value |= 0x800000;
    } else if opcode.vex == 1 {
        value |= 0x1000000 + 0x800000 * vexl_value(opcode)?;
    } else if opcode.vex == 2 {
        value |= 0x2000000;
        if variant.evexsae == 0 {
            value |= 0x800000 * vexl_value(opcode)?;
        }
    }
    if variant.evexsae != 0 && variant.evexbcst {
        return Err("SAE and broadcast cannot share an encoding".to_owned());
    }
    if variant.evexsae != 0 || variant.evexbcst {
        value |= 0x4000000;
    }
    if desc.flags.contains("VSIB") {
        value |= 0x8000000;
    }
    if variant.evexmask == 2 {
        value |= 0x1000000000;
    }
    if matches!(variant.downgrade, 1 | 2) {
        value |= 0x4000000000;
    }
    if variant.downgrade == 2 {
        value |= 0x40000000000;
    }
    if variant.evexdisp8scale < 0 {
        return Err("negative EVEX disp8 scale".to_owned());
    }
    value |= 0x8000000000 * variant.evexdisp8scale as u64;
    if alt >= 0x100 {
        return Err("encode alternate bits exhausted".to_owned());
    }
    value |= high_regs << 45;
    if flags.imm_control >= 3 {
        let imm_size = desc.imm_size(opsize / 8)?;
        if imm_size < 0 {
            return Err("negative immediate size".to_owned());
        }
        value |= (imm_size as u64) << 47;
    } else if matches!(flags.imm_control, 1 | 2) {
        value |= 1 << 47;
    }
    let mut enc_encoding = desc.encoding.as_str();
    if enc_encoding != "I" && enc_encoding.ends_with('I') {
        enc_encoding = &enc_encoding[..enc_encoding.len() - 1];
    } else if enc_encoding == "IA" {
        enc_encoding = "A";
    }
    let index = [
        "NP", "M", "R", "M1", "MC", "MR", "RM", "RMA", "MRC", "AM", "MA", "I", "O", "OA", "S", "A",
        "D", "FD", "TD", "IM", "RVM", "RVMR", "RMV", "VM", "MVR", "MRV",
    ]
    .iter()
    .position(|&name| name == enc_encoding)
    .ok_or_else(|| format!("unknown compact encoding {enc_encoding}"))?;
    value |= (index as u64) << 51;
    value |= (alt as u64) << 56;
    Ok(value)
}

fn vexl_value(opcode: &Opcode) -> Result<u64> {
    opcode
        .vexl
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("0")
        .parse()
        .map_err(|_| format!("invalid specialized VEX.L {:?}", opcode.vexl))
}

pub fn generate(entries: &[Entry]) -> Result<EncodeOutput> {
    let mut mnemonics = encode_mnems(entries)?;
    let nop = EncodeVariant::new(Opcode::parse("90")?, InstrDesc::parse("NP - - - - NOP")?);
    if let Some(existing) = mnemonics
        .iter_mut()
        .find(|m| m.name == "NOP" && m.opsize == 0 && m.ots.is_empty())
    {
        existing.variants = vec![nop];
    } else {
        mnemonics.push(Mnemonic {
            name: "NOP".to_owned(),
            opsize: 0,
            ots: String::new(),
            variants: vec![nop],
        });
    }
    let mut values: Vec<(String, u64)> = Vec::new();
    let mut value_indices = BTreeMap::new();
    let mut alt_table = vec![0];
    for mnemonic in &mnemonics {
        let variants = &mnemonic.variants;
        let first = variants
            .first()
            .ok_or_else(|| format!("empty mnemonic {}", mnemonic.name))?;
        let mut high_regs = 0;
        if matches!(first.desc.mnemonic.as_str(), "MOVSX" | "MOVZX") || mnemonic.opsize == 8 {
            for (i, (ot, op)) in mnemonic.ots.chars().zip(&first.desc.operands).enumerate() {
                if ot == 'r'
                    && op.kind() == "GP"
                    && op.abssize(Some(mnemonic.opsize / 8), None)? == 1
                {
                    high_regs |= 1 << i;
                }
            }
        }
        let mut count = variants.len();
        let mut jump_alternative = false;
        if first.desc.encoding == "D" {
            if count > 2 {
                return Err(format!(
                    "too many direct branch encodings {}",
                    mnemonic.name
                ));
            }
            if count == 2 {
                if !first.desc.mnemonic.starts_with('J') {
                    return Err(format!(
                        "non-jump with direct branch alternatives {}",
                        mnemonic.name
                    ));
                }
                count = 1;
                jump_alternative = true;
            }
        }
        let alt_start = alt_table.len();
        for (i, variant) in variants[..count].iter().enumerate() {
            let alt = if jump_alternative {
                0xff
            } else if i + 1 == count {
                0
            } else {
                alt_start + i
            };
            let value = encode_variant(variant, mnemonic.opsize, high_regs, alt)?;
            if i == 0 {
                let name = format!("FE_{}", mnemonic.name);
                if let Some(&index) = value_indices.get(&name) {
                    values[index] = (name, value);
                } else {
                    value_indices.insert(name.clone(), values.len());
                    values.push((name, value));
                }
            } else {
                alt_table.push(value);
            }
        }
    }
    let mut public_rs = String::new();
    let mut private_rs = format!("pub static FE_ALT_TABLE: [u64; {}] = [\n", alt_table.len());
    let mut semantic_tsv = String::new();
    for (name, value) in values {
        writeln!(public_rs, "pub const {name}: u64 = {value:#x};").unwrap();
        writeln!(semantic_tsv, "F\t{name}\t{value}").unwrap();
    }
    for (i, value) in alt_table.into_iter().enumerate() {
        writeln!(private_rs, "    {value:#x},").unwrap();
        writeln!(semantic_tsv, "A\t{i}\t{value}").unwrap();
    }
    private_rs.push_str("];\n");
    Ok(EncodeOutput {
        public_rs,
        private_rs,
        semantic_tsv,
    })
}

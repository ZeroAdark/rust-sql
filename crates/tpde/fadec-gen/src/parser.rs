// SPDX-FileCopyrightText: 2018 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

use crate::Result;
use crate::model::{Entry, InstrDesc, OpKind, Opcode, encoding, is_whitespace, op_order};

impl Opcode {
    pub fn parse(input: &str) -> Result<Self> {
        let invalid = || format!("invalid opcode: {input}");
        let mut rest = input.strip_suffix('\n').unwrap_or(input);
        let mut vex = 0;
        if let Some(suffix) = rest.strip_prefix("EVEX.") {
            vex = 2;
            rest = suffix;
        } else if let Some(suffix) = rest.strip_prefix("VEX.") {
            vex = 1;
            rest = suffix;
        }
        let mut prefix = None;
        for candidate in ["NP", "66", "F2", "F3", "NFx"] {
            if let Some(suffix) = rest
                .strip_prefix(candidate)
                .and_then(|s| s.strip_prefix('.'))
            {
                prefix = Some(candidate.to_owned());
                rest = suffix;
                break;
            }
        }
        if vex != 0 && prefix.is_none() {
            return Err(invalid());
        }
        let mut rexw = None;
        let mut vexl = None;
        if prefix.is_some() {
            for candidate in ["0", "1"] {
                if let Some(suffix) = rest
                    .strip_prefix('W')
                    .and_then(|s| s.strip_prefix(candidate))
                    .and_then(|s| s.strip_prefix('.'))
                {
                    rexw = Some(candidate.to_owned());
                    rest = suffix;
                    break;
                }
            }
            for candidate in ["0", "1", "12", "2", "IG"] {
                if let Some(suffix) = rest
                    .strip_prefix('L')
                    .and_then(|s| s.strip_prefix(candidate))
                    .and_then(|s| s.strip_prefix('.'))
                {
                    vexl = Some(candidate.to_owned());
                    rest = suffix;
                    break;
                }
            }
        }
        for (candidate, escape) in [
            ("0f38", 2),
            ("0f3a", 3),
            ("0f", 1),
            ("M5.", 5),
            ("M6.", 6),
            ("M7.", 7),
            ("", 0),
        ] {
            if let Some(suffix) = rest.strip_prefix(candidate) {
                if let Some((opc, extended, modrm)) = parse_opcode_tail(suffix) {
                    return Ok(Self {
                        prefix,
                        escape,
                        opc,
                        extended,
                        modrm,
                        vex,
                        vexl,
                        rexw,
                    });
                }
            }
        }
        Err(invalid())
    }
}

type OpcodeTail = (u8, bool, (Option<char>, Option<u8>, Option<u8>));

fn parse_opcode_tail(mut rest: &str) -> Option<OpcodeTail> {
    let extended = rest.ends_with('+');
    if extended {
        rest = &rest[..rest.len() - 1];
    }
    if rest.len() < 2 || !rest.as_bytes()[..2].iter().all(|byte| is_hex(*byte)) {
        return None;
    }
    let opc = u8::from_str_radix(&rest[..2], 16).ok()?;
    rest = &rest[2..];
    let modrm = if rest.is_empty() {
        (None, None, None)
    } else if let Some(spec) = rest.strip_prefix('/') {
        match spec.as_bytes() {
            [fixed @ (b'r' | b'm')] => (Some(char::from(*fixed)), None, None),
            [fixed @ (b'r' | b'm'), rm @ b'0'..=b'7'] => {
                (Some(char::from(*fixed)), None, Some(rm - b'0'))
            }
            [reg @ b'0'..=b'7'] => (None, Some(reg - b'0'), None),
            [reg @ b'0'..=b'7', fixed @ (b'r' | b'm')] => {
                (Some(char::from(*fixed)), Some(reg - b'0'), None)
            }
            _ => return None,
        }
    } else if rest.len() == 2
        && matches!(rest.as_bytes()[0], b'c'..=b'f')
        && is_hex(rest.as_bytes()[1])
    {
        let extension = u8::from_str_radix(rest, 16).ok()?;
        (Some('r'), Some((extension >> 3) & 7), Some(extension & 7))
    } else {
        return None;
    };
    Some((opc, extended, modrm))
}

fn is_hex(byte: u8) -> bool {
    matches!(byte, b'0'..=b'9' | b'a'..=b'f')
}

pub fn parse_table(input: &str, with_undoc: bool) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for (offset, line) in table_lines(input).enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line_number = offset + 1;
        let (line, weak) = line
            .strip_prefix('*')
            .map_or((line, false), |line| (line, true));
        let parse_line = || {
            let line = line.trim_start_matches(is_whitespace);
            let boundary = line
                .find(is_whitespace)
                .ok_or("missing instruction description")?;
            let opcode = Opcode::parse(&line[..boundary])?;
            let desc = InstrDesc::parse(line[boundary..].trim_start_matches(is_whitespace))?;
            verify_opcode_desc(&opcode, &desc)?;
            Ok((opcode, desc))
        };
        let (opcode, desc) =
            parse_line().map_err(|err: String| format!("line {line_number}: {err}"))?;
        if with_undoc || !desc.flags.contains("UNDOC") {
            entries.push(Entry {
                line: line_number,
                weak,
                opcode,
                desc,
            });
        }
    }
    Ok(entries)
}

pub(crate) fn table_lines(mut input: &str) -> impl Iterator<Item = &str> {
    std::iter::from_fn(move || {
        if input.is_empty() {
            return None;
        }
        if let Some((offset, boundary)) = input.char_indices().find(|(_, c)| {
            matches!(
                c,
                '\n' | '\r' | '\u{b}' | '\u{c}' | '\u{1c}'
                    ..='\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}'
            )
        }) {
            let line = &input[..offset];
            input = &input[offset + boundary.len_utf8()..];
            if boundary == '\r' {
                input = input.strip_prefix('\n').unwrap_or(input);
            }
            Some(line)
        } else {
            let line = input;
            input = "";
            Some(line)
        }
    })
}

pub fn verify_opcode_desc(opcode: &Opcode, desc: &InstrDesc) -> Result<()> {
    let fail = |message: &str| Err(format!("{message} {opcode:?}, {desc:?}"));
    let flags = encoding(&desc.encoding)?;
    let order = op_order(&desc.encoding)?;
    let expected_imm = ["", "I", "O", "L", "IA", "", "J"][usize::from(flags.imm_control)];
    let fixed_mod = if opcode.extended
        || matches!(
            desc.mnemonic.as_str(),
            "MOV_CR2G" | "MOV_DR2G" | "MOV_G2CR" | "MOV_G2DR"
        ) {
        Some('r')
    } else {
        opcode.modrm.0
    };
    let expected_modrm = match fixed_mod {
        None => "EQWFKT",
        Some('r') => "RNUFKT",
        Some('m') => "M",
        _ => return fail("invalid fixed ModRM mode"),
    };
    let expected_vex = if opcode.vex != 0 { "BHKT" } else { "BHRF" };
    for (i, operand) in desc.operands.iter().enumerate() {
        let name = *order.get(i).ok_or("too many instruction operands")?;
        let expected = match name {
            "modrm" => expected_modrm,
            "modreg" => "GPVSCDFKT",
            "vexreg" => expected_vex,
            "imm" => expected_imm,
            _ => unreachable!(),
        };
        if !expected.contains(operand.regkind) {
            return fail(&format!("{name} operand-regkind mismatch"));
        }
    }
    if desc.flags.contains("INSTR_WIDTH") && desc.operands.len() > 3 {
        return fail("+w with four operands");
    }
    if opcode.escape == 2 && flags.imm_control != 0 {
        return fail("0f38 has no immediate operand");
    }
    if opcode.escape == 3 && desc.imm_size(4)? != 1 {
        return fail("0f3a must have immediate byte");
    }
    if opcode.escape == 0 && opcode.prefix.is_some() {
        return fail("unescaped opcode has prefix");
    }
    if opcode.escape == 0 && opcode.vexl.is_some() {
        return fail("unescaped opcode has L specifier");
    }
    if opcode.escape == 0 && opcode.rexw.is_some() {
        return fail("unescaped opcode has W specifier");
    }
    if opcode.escape == 0 && opcode.vex != 0 {
        return fail("VEX opcode without escape");
    }
    if opcode.vex != 0 && opcode.extended {
        return fail("VEX/EVEX must not be extended");
    }
    if opcode.vex != 0 && !matches!(opcode.prefix.as_deref(), Some("NP" | "66" | "F2" | "F3")) {
        return fail("VEX/EVEX must have mandatory prefix");
    }
    if opcode.vexl.as_deref() == Some("IG")
        && desc.dynsizes()?.iter().any(|size| *size != OpKind::SZ_OP)
    {
        return fail("(E)VEX.LIG with dynamic vector size");
    }
    if desc.flags.contains("VSIB") && opcode.modrm.0 != Some('m') {
        return fail("VSIB for non-memory opcode");
    }
    if opcode.vex == 2
        && flags.vexreg_idx != 0
        && desc.operand(flags.vexreg_idx ^ 3)?.kind() != "XMM"
    {
        return fail("EVEX.vvvv must refer to XMM");
    }
    if opcode.vex == 2
        && flags.modreg_idx != 0
        && flags.modreg_idx ^ 3 != 0
        && desc.operand(flags.modreg_idx ^ 3)?.kind() == "MASK"
    {
        return fail("ModRM.reg mask not first operand");
    }
    if opcode.vex == 2 && opcode.modrm.0 != Some('r') {
        let mut tuples = desc.flags.iter().filter(|flag| flag.starts_with("TUPLE"));
        let Some(tuple) = tuples.next() else {
            return fail("missing tuple type in");
        };
        if tuples.next().is_some() {
            return fail("missing tuple type in");
        }
        if flags.modrm_idx == 0 {
            return fail("missing memory operand");
        }
        let (broadcast, evexw, sizes) = tuple_type(tuple)?;
        if desc.flags.contains("BCST") {
            if broadcast.is_none() {
                return fail("broadcast on incompatible type");
            }
            if desc.flags.contains("BCST16") != (broadcast == Some(2)) {
                return fail(&format!(
                    "bcst16 mismatch, should be {}",
                    broadcast.unwrap()
                ));
            }
        }
        if evexw.is_some() && opcode.rexw.as_deref() != evexw {
            return fail("incompatible EVEX.W");
        }
        let operand = desc.operand(flags.modrm_idx ^ 3)?;
        for (length, tuple_size) in sizes.into_iter().enumerate() {
            let operand_size = operand.abssize(Some(0), Some(16 << length))?;
            if let Some(tuple_size) = tuple_size {
                if operand_size != tuple_size {
                    return fail(&format!("memory size {operand_size} != {tuple_size}"));
                }
            }
        }
    }
    Ok(())
}

type TupleType = (Option<i32>, Option<&'static str>, [Option<i32>; 3]);

fn tuple_type(name: &str) -> Result<TupleType> {
    let (broadcast, width, sizes) = match name {
        "TUPLE_FULL_16" => (Some(2), Some("0"), [Some(16), Some(32), Some(64)]),
        "TUPLE_FULL_32" => (Some(4), Some("0"), [Some(16), Some(32), Some(64)]),
        "TUPLE_FULL_64" => (Some(8), Some("1"), [Some(16), Some(32), Some(64)]),
        "TUPLE_HALF_16" => (Some(2), Some("0"), [Some(8), Some(16), Some(32)]),
        "TUPLE_HALF_32" => (Some(4), Some("0"), [Some(8), Some(16), Some(32)]),
        "TUPLE_HALF_64" => (Some(8), Some("1"), [Some(8), Some(16), Some(32)]),
        "TUPLE_QUARTER_16" => (Some(2), Some("0"), [Some(4), Some(8), Some(16)]),
        "TUPLE_FULL_MEM" | "TUPLE_MOVDDUP" => (None, None, [Some(16), Some(32), Some(64)]),
        "TUPLE_HALF_MEM" => (None, None, [Some(8), Some(16), Some(32)]),
        "TUPLE_QUARTER_MEM" => (None, None, [Some(4), Some(8), Some(16)]),
        "TUPLE_EIGHTH_MEM" => (None, None, [Some(2), Some(4), Some(8)]),
        "TUPLE1_SCALAR_8" => (None, None, [Some(1), Some(1), Some(1)]),
        "TUPLE1_SCALAR_16" => (None, None, [Some(2), Some(2), Some(2)]),
        "TUPLE1_SCALAR_32" => (None, Some("0"), [Some(4), Some(4), Some(4)]),
        "TUPLE1_SCALAR_64" => (None, Some("1"), [Some(8), Some(8), Some(8)]),
        "TUPLE1_SCALAR_OPSZ" => (None, None, [Some(0), Some(0), Some(0)]),
        "TUPLE1_FIXED_32" => (None, None, [Some(4), Some(4), Some(4)]),
        "TUPLE1_FIXED_64" => (None, None, [Some(8), Some(8), Some(8)]),
        "TUPLE2_32" => (None, Some("0"), [Some(8), Some(8), Some(8)]),
        "TUPLE2_64" => (None, Some("1"), [None, Some(16), Some(16)]),
        "TUPLE4_32" => (None, Some("0"), [None, Some(16), Some(16)]),
        "TUPLE4_64" => (None, Some("1"), [None, None, Some(32)]),
        "TUPLE8_32" => (None, Some("0"), [None, None, Some(32)]),
        "TUPLE_MEM128" => (None, None, [Some(16), Some(16), Some(16)]),
        _ => return Err(format!("unknown tuple type: {name}")),
    };
    Ok((broadcast, width, sizes))
}

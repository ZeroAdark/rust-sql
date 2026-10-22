// SPDX-FileCopyrightText: 2018 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

use std::collections::BTreeSet;
use std::fmt::Write;

use crate::Result;
use crate::encode::{EncodeVariant, Mnemonic, encode_mnems};
use crate::model::{Entry, encoding};

pub struct Encode2Output {
    pub public_rs: String,
    pub private_rs: String,
    pub semantic_tsv: String,
}

pub fn unique<T: Eq>(values: impl IntoIterator<Item = T>) -> Result<T> {
    let mut values = values.into_iter();
    let first = values.next().ok_or("empty unique-value set")?;
    if values.any(|value| value != first) {
        return Err("multiple values in unique-value set".into());
    }
    Ok(first)
}

#[derive(Default)]
struct Code {
    rust: String,
    trace: String,
}

impl Code {
    fn emit(&mut self, trace: impl AsRef<str>, rust: impl AsRef<str>) {
        self.trace
            .extend(trace.as_ref().chars().filter(|c| !c.is_ascii_whitespace()));
        self.rust.push_str(rust.as_ref());
        self.rust.push('\n');
    }

    fn put(&mut self, trace: impl AsRef<str>, rust: impl AsRef<str>) {
        self.emit(
            format!("buf[idx++] = {};", trace.as_ref()),
            format!("buf[idx] = ({}) as u8; idx += 1;", rust.as_ref()),
        );
    }
}

struct Expr {
    c: String,
    rust: String,
}

impl Expr {
    fn same(value: impl Into<String>) -> Self {
        let value = value.into();
        Self {
            c: value.clone(),
            rust: value,
        }
    }

    fn reg(index: u8) -> Self {
        Self {
            c: format!("op_reg_idx(op{index})"),
            rust: format!("u32::from(op{index}.idx)"),
        }
    }
}

fn encode2_gen_legacy(
    code: &mut Code,
    variant: &EncodeVariant,
    opsize: i32,
    supports_high_regs: &[usize],
    imm: &Expr,
    immsz: &Expr,
    has_idx: bool,
) -> Result<()> {
    let opcode = &variant.opcode;
    let desc = &variant.desc;
    let enc = encoding(&desc.encoding)?;
    let rexw = opcode.rexw.as_deref() == Some("1");
    let mut rex = Vec::new();
    let mut rex_values = BTreeSet::new();
    if rexw {
        rex.push(Expr::same("0x48"));
    }
    for i in supports_high_regs {
        rex.push(Expr {
            c: format!("(op_reg_idx(op{i}) >= 4 && op_reg_idx(op{i}) <= 15?0x40:0)"),
            rust: format!("(if op{i}.idx >= 4 && op{i}.idx <= 15 {{ 0x40 }} else {{ 0 }})"),
        });
        rex_values.insert(0x40);
    }
    let has_modreg_rex = enc.modreg_idx != 0
        && matches!(
            desc.operands[(enc.modreg_idx ^ 3) as usize].kind(),
            "GP" | "XMM" | "CR" | "DR"
        );
    if enc.modrm_idx != 0 {
        if opcode.modrm.0 == Some('m') {
            let reg = if has_modreg_rex {
                Expr::reg(enc.modreg_idx ^ 3)
            } else {
                Expr::same("0")
            };
            rex.push(Expr {
                c: format!("enc_rex_mem(op{}, {})", enc.modrm_idx ^ 3, reg.c),
                rust: format!("R::enc_rex_mem(op{}, {})", enc.modrm_idx ^ 3, reg.rust),
            });
            rex_values.extend([0x41, 0x42, 0x44]);
        } else if matches!(
            desc.operands[(enc.modrm_idx ^ 3) as usize].kind(),
            "GP" | "XMM"
        ) {
            let i = enc.modrm_idx ^ 3;
            rex.push(Expr {
                c: format!("(op_reg_idx(op{i})&8?0x1:0)"),
                rust: format!("(if op{i}.idx & 8 != 0 {{ 0x1 }} else {{ 0 }})"),
            });
            rex_values.insert(0x41);
            if has_modreg_rex {
                let i = enc.modreg_idx ^ 3;
                rex.push(Expr {
                    c: format!("(op_reg_idx(op{i})&8?0x4:0)"),
                    rust: format!("(if op{i}.idx & 8 != 0 {{ 0x4 }} else {{ 0 }})"),
                });
                rex_values.insert(0x44);
            }
        }
    } else if has_modreg_rex {
        let i = enc.modreg_idx ^ 3;
        rex.push(Expr {
            c: format!("(op_reg_idx(op{i})&8?0x1:0)"),
            rust: format!("(if op{i}.idx & 8 != 0 {{ 0x1 }} else {{ 0 }})"),
        });
        rex_values.insert(0x41);
    }
    if !rex.is_empty() {
        code.emit(
            format!(
                "unsigned rex = {};",
                rex.iter()
                    .map(|e| e.c.as_str())
                    .collect::<Vec<_>>()
                    .join("|")
            ),
            format!(
                "let rex: u32 = {};",
                rex.iter()
                    .map(|e| e.rust.as_str())
                    .collect::<Vec<_>>()
                    .join("|")
            ),
        );
    }
    for i in supports_high_regs {
        code.emit(
            format!("if (rex && op_reg_gph(op{i})) return 0;"),
            format!("if rex != 0 && (op{i}.idx & !0x3) == 0x24 {{ return 0; }}"),
        );
    }
    if !has_idx {
        code.emit("unsigned idx = 0;", "let mut idx: usize = 0;");
    }
    if opcode.prefix.as_deref() == Some("LOCK") {
        code.put("0xF0", "0xF0");
    }
    if opsize == 16 || opcode.prefix.as_deref() == Some("66") {
        code.put("0x66", "0x66");
    }
    if let Some(prefix @ ("F2" | "F3")) = opcode.prefix.as_deref() {
        code.put(format!("0x{prefix}"), format!("0x{prefix}"));
    }
    if rexw {
        code.put("rex", "rex");
    } else if rex_values.len() == 1 {
        let value = rex_values.first().unwrap();
        code.emit(
            format!("buf[idx] = {value}; idx += rex != 0;"),
            format!("buf[idx] = {value}; idx += usize::from(rex != 0);"),
        );
    } else if rex_values.len() == 2 {
        code.emit(
            "buf[idx] = 0x40|rex; idx += rex != 0;",
            "buf[idx] = (0x40 | rex) as u8; idx += usize::from(rex != 0);",
        );
    } else if !rex.is_empty() {
        code.emit(
            "if (rex) buf[idx++] = 0x40|rex;",
            "if rex != 0 { buf[idx] = (0x40 | rex) as u8; idx += 1; }",
        );
    }
    if opcode.escape != 0 {
        code.put("0x0F", "0x0F");
        match opcode.escape {
            2 => code.put("0x38", "0x38"),
            3 => code.put("0x3A", "0x3A"),
            _ => {}
        }
    }
    let opcodestr = format!(
        "{:#x}{}",
        opcode.opc,
        if variant.flexcc { "|(flags>>16)" } else { "" }
    );
    code.put(&opcodestr, &opcodestr);
    if let (Some(_), Some(reg), Some(rm)) = opcode.modrm {
        let extension = 0xc0 | (reg << 3) | rm;
        code.put(format!("{extension:#x}"), format!("{extension:#x}"));
    }
    if enc.modrm != 0 {
        let reg = if enc.modreg_idx != 0 {
            Expr::reg(enc.modreg_idx ^ 3)
        } else {
            Expr::same(opcode.modrm.1.unwrap_or(0).to_string())
        };
        if opcode.modrm.0 == Some('m') {
            if desc.flags.contains("VSIB") || opcode.modrm.2.is_some() {
                return Err("legacy memory encoding requires non-VSIB and no fixed r/m".into());
            }
            let rm = enc.modrm_idx ^ 3;
            code.emit(
                format!(
                    "idx = enc_mem(buf+idx, idx+{}, op{rm}, {}, 0, 0);",
                    immsz.c, reg.c
                ),
                format!(
                    "idx = R::enc_mem(&mut buf[idx..], idx+{}, op{rm}, {}, 0, 0);",
                    immsz.rust, reg.rust
                ),
            );
            code.emit("if (!idx) return 0;", "if idx == 0 { return 0; }");
            code.emit(
                format!("idx -= {};", immsz.c),
                format!("idx -= {};", immsz.rust),
            );
        } else {
            let rm = if enc.modrm_idx != 0 {
                Expr::reg(enc.modrm_idx ^ 3)
            } else {
                Expr::same(opcode.modrm.2.unwrap_or(0).to_string())
            };
            code.put(
                format!("0xC0|({}<<3)|({}&7)", reg.c, rm.c),
                format!("0xC0|({}<<3)|({}&7)", reg.rust, rm.rust),
            );
        }
    } else if enc.modrm_idx != 0 {
        let i = enc.modrm_idx ^ 3;
        code.emit(
            format!("buf[idx-1] |= op_reg_idx(op{i}) & 7;"),
            format!("buf[idx-1] |= op{i}.idx & 7;"),
        );
    }
    if enc.imm_control >= 2 {
        let (value_c, value_rust) = if enc.imm_control == 6 {
            (
                format!("{} - idx", imm.c),
                format!("({}).wrapping_sub(idx as i64)", imm.rust),
            )
        } else {
            (imm.c.clone(), imm.rust.clone())
        };
        code.emit(
            format!("enc_imm(buf+idx, {value_c}, {});", immsz.c),
            format!("R::enc_imm(&mut buf[idx..], {value_rust}, {});", immsz.rust),
        );
        code.emit(
            format!("return idx + {};", immsz.c),
            format!("return idx + {};", immsz.rust),
        );
    } else {
        code.emit("return idx;", "return idx;");
    }
    Ok(())
}

fn encode2_gen_vex(
    code: &mut Code,
    variant: &EncodeVariant,
    imm: &Expr,
    immsz: &Expr,
    has_idx: bool,
) -> Result<()> {
    let opcode = &variant.opcode;
    let enc = encoding(&variant.desc.encoding)?;
    let pp = match opcode.prefix.as_deref() {
        Some("NP") => 0,
        Some("66") => 1,
        Some("F3") => 2,
        Some("F2") => 3,
        _ => return Err("invalid VEX mandatory prefix".into()),
    };
    let mut helperopc = u32::from(opcode.opc) << 16 | pp << 8;
    if opcode.rexw.as_deref() == Some("1") {
        helperopc |= 0x8000;
    }
    if variant.evexsae == 0 {
        let vexl = opcode
            .vexl
            .as_deref()
            .unwrap_or("0")
            .parse::<u32>()
            .map_err(|_| "invalid concrete VEX length")?;
        helperopc |= 0x20 * vexl;
    }
    helperopc |= u32::from(opcode.escape) << 10;
    if variant.evexsae != 0 || variant.evexbcst {
        helperopc |= 0x10;
    }
    if variant.evexmask == 2 {
        helperopc |= 0x80;
    }
    if matches!(variant.downgrade, 1 | 2) {
        helperopc |= 0x1000000;
    }
    if variant.downgrade == 2 {
        helperopc |= 0x2000000;
    }
    let mut helperopc = Expr::same(format!("{helperopc:#x}"));
    if variant.flexcc {
        helperopc.c.push_str("|(flags&FE_CC_MASK)");
        helperopc.rust.push_str("|(flags&FE_CC_MASK)");
    }
    if variant.evexsae == 2 {
        helperopc.c.push_str("|(flags&FE_RC_MASK)");
        helperopc.rust.push_str("|(flags&FE_RC_MASK)");
    }
    if variant.evexmask != 0 {
        code.emit(
            "if (!op_reg_idx(opmask)) return 0;",
            "if opmask.idx == 0 { return 0; }",
        );
        helperopc.c.push_str("|(op_reg_idx(opmask)&7)");
        helperopc.rust.push_str("|(u32::from(opmask.idx)&7)");
    }
    let reg = if enc.modreg_idx != 0 {
        Expr::reg(enc.modreg_idx ^ 3)
    } else {
        Expr::same(opcode.modrm.1.unwrap_or(0).to_string())
    };
    let vexop = if enc.vexreg_idx != 0 {
        Expr::reg(enc.vexreg_idx ^ 3)
    } else {
        Expr::same("0")
    };
    let is_memory = opcode.modrm.0 == Some('m');
    let (helper, args) = if enc.modrm == 0 && opcode.modrm == (None, None, None) {
        if opcode.vex != 1 || has_idx {
            return Err("prefix-only VEX requires VEX and no outer prefixes".into());
        }
        ("enc_vex_common".to_owned(), Expr::same("0, 0, 0, 0"))
    } else if is_memory {
        if !matches!(opcode.modrm.2, None | Some(4)) {
            return Err("VEX memory fixed r/m must be absent or SIB".into());
        }
        let family = match opcode.vex {
            1 => "_vex",
            2 => "_evex",
            _ => return Err("invalid VEX family".into()),
        };
        let suffix = if variant.desc.flags.contains("VSIB") {
            "_vsib"
        } else {
            "_mem"
        };
        let rm = enc.modrm_idx ^ 3;
        let force = u8::from(opcode.modrm.2 == Some(4));
        let idx = if has_idx { "+idx" } else { "" };
        (
            format!("enc{family}{suffix}"),
            Expr {
                c: format!(
                    "op{rm}, {}, {}, {}{idx}, {force}, {}",
                    reg.c, vexop.c, immsz.c, variant.evexdisp8scale
                ),
                rust: format!(
                    "op{rm}, {}, {}, {}{idx}, {force}, {}",
                    reg.rust, vexop.rust, immsz.rust, variant.evexdisp8scale
                ),
            },
        )
    } else {
        let rm = if enc.modrm_idx != 0 {
            Expr::reg(enc.modrm_idx ^ 3)
        } else {
            Expr::same(opcode.modrm.2.unwrap_or(0).to_string())
        };
        let suffix = if opcode.vex == 2
            && enc.modrm_idx != 0
            && variant.desc.operands[(enc.modrm_idx ^ 3) as usize].kind() == "XMM"
        {
            "_xmm"
        } else {
            "_reg"
        };
        let family = match opcode.vex {
            1 => "_vex",
            2 => "_evex",
            _ => return Err("invalid VEX family".into()),
        };
        (
            format!("enc{family}{suffix}"),
            Expr {
                c: format!("{}, {}, {}", rm.c, reg.c, vexop.c),
                rust: format!("{}, {}, {}", rm.rust, reg.rust, vexop.rust),
            },
        )
    };
    let buf_c = if has_idx { "buf+idx" } else { "buf" };
    let buf_rust = if has_idx {
        "&mut buf[idx..]"
    } else {
        "&mut buf[..]"
    };
    let call = Expr {
        c: format!("{helper}({buf_c}, {}, {})", helperopc.c, args.c),
        rust: format!("R::{helper}({buf_rust}, {}, {})", helperopc.rust, args.rust),
    };
    if enc.imm_control >= 2 {
        if enc.imm_control >= 6 {
            return Err("relative jumps cannot have VEX/EVEX".into());
        }
        if is_memory {
            code.emit(
                format!("unsigned instlen = {};", call.c),
                format!("let instlen = {};", call.rust),
            );
            code.emit(
                format!(
                    "if (instlen) enc_imm(buf+instlen-{}, {}, {});",
                    immsz.c, imm.c, immsz.c
                ),
                format!(
                    "if instlen != 0 {{ R::enc_imm(&mut buf[instlen-{}..], {}, {}); }}",
                    immsz.rust, imm.rust, immsz.rust
                ),
            );
            code.emit("return instlen;", "return instlen;");
        } else {
            code.emit(
                format!("unsigned vexoff = {};", call.c),
                format!("let vexoff = {};", call.rust),
            );
            let offset = if has_idx { "idx+vexoff" } else { "vexoff" };
            code.emit(
                format!("enc_imm({buf_c}+vexoff, {}, {});", imm.c, immsz.c),
                format!(
                    "R::enc_imm(&mut buf[{offset}..], {}, {});",
                    imm.rust, immsz.rust
                ),
            );
            let idx = if has_idx { "+idx" } else { "" };
            code.emit(
                format!("return vexoff ? vexoff+{}{idx} : 0;", immsz.c),
                format!(
                    "return if vexoff != 0 {{ vexoff+{}{idx} }} else {{ 0 }};",
                    immsz.rust
                ),
            );
        }
    } else if has_idx && !is_memory {
        code.emit(
            format!("unsigned vexoff = {};", call.c),
            format!("let vexoff = {};", call.rust),
        );
        code.emit(
            "return vexoff ? vexoff+idx : 0;",
            "return if vexoff != 0 { vexoff+idx } else { 0 };",
        );
    } else {
        code.emit(
            format!("return {};", call.c),
            format!("return {};", call.rust),
        );
    }
    Ok(())
}

fn emit_mnemonic(mnem: &Mnemonic, output: &mut Encode2Output) -> Result<()> {
    let variants = &mnem.variants;
    let first = variants.first().ok_or("empty encode2 mnemonic")?;
    let opsize = mnem.opsize;
    let max_imm_size = variants
        .iter()
        .map(|v| v.desc.imm_size(opsize / 8))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .max()
        .unwrap();
    let mut high_regs = Vec::new();
    if matches!(first.desc.mnemonic.as_str(), "MOVSX" | "MOVZX") || opsize == 8 {
        for (i, (ot, op)) in mnem.ots.chars().zip(&first.desc.operands).enumerate() {
            if ot == 'r' && op.kind() == "GP" && op.abssize(Some(opsize / 8), None)? == 1 {
                high_regs.push(i);
            }
        }
    }
    let vsib = unique(variants.iter().map(|v| v.desc.flags.contains("VSIB")))?;
    let opkinds = unique(
        variants
            .iter()
            .map(|v| v.desc.operands.iter().map(|o| o.kind()).collect::<Vec<_>>()),
    )?;
    let mask = unique(variants.iter().map(|v| v.evexmask))?;
    unique(variants.iter().map(|v| v.evexsae))?;
    let mut op_tys = Vec::new();
    let mut rust_tys = Vec::new();
    for (i, (ot, kind)) in mnem.ots.chars().zip(opkinds).enumerate() {
        let reg_ty = match kind {
            "FPU" => "ST",
            "SEG" => "SREG",
            "MMX" => "MM",
            other => other,
        };
        let (c, rust) = match ot {
            'i' => {
                let bits = if max_imm_size == 3 {
                    32
                } else {
                    max_imm_size * 8
                };
                if !matches!(bits, 8 | 16 | 32 | 64) {
                    return Err(format!("invalid immediate width {bits}"));
                }
                (format!("int{bits}_t"), format!("i{bits}"))
            }
            'a' => ("uintptr_t".into(), "usize".into()),
            'r' => {
                let ty = format!(
                    "FeReg{}",
                    if high_regs.contains(&i) {
                        "GPLH"
                    } else {
                        reg_ty
                    }
                );
                let rust = if high_regs.contains(&i) {
                    "impl Into<FeRegGPLH>".into()
                } else {
                    ty.clone()
                };
                (ty, rust)
            }
            'k' => ("FeRegMASK".into(), "FeRegMASK".into()),
            'm' => {
                let ty = if vsib { "FeMemV" } else { "FeMem" };
                (ty.into(), ty.into())
            }
            'b' => ("FeMem".into(), "FeMem".into()),
            'o' => ("const void*".into(), "usize".into()),
            _ => return Err(format!("unknown encode2 operand type {ot}")),
        };
        op_tys.push(c);
        rust_tys.push(rust);
    }
    let fnname = format!("fe64_{}", mnem.name);
    let mut c_args = if mask != 0 {
        ", FeRegMASK opmask".to_owned()
    } else {
        String::new()
    };
    let mut rust_args = if mask != 0 {
        ", opmask: FeRegMASK".to_owned()
    } else {
        String::new()
    };
    for (i, (c, rust)) in op_tys.iter().zip(&rust_tys).enumerate() {
        write!(c_args, ", {c} op{i}").unwrap();
        write!(rust_args, ", op{i}: {rust}").unwrap();
    }
    let mut code = Code::default();
    code.emit(
        format!("unsigned ({fnname})(uint8_t* buf, int flags{c_args}) {{"),
        format!("#[allow(non_snake_case, unused_variables, unused_mut, unused_parens)]\n#[inline]\npub fn {fnname}<R: Encode2Runtime>(buf: &mut [u8; 32], address: usize, flags: u32{rust_args}) -> usize {{"),
    );
    for i in &high_regs {
        writeln!(code.rust, "let op{i}: FeRegGPLH = op{i}.into();").unwrap();
    }
    let memory = unique(variants.iter().map(|v| v.opcode.modrm.0 == Some('m')))?;
    let useg = unique(variants.iter().map(|v| v.desc.flags.contains("USEG")))?;
    let u67 = unique(variants.iter().map(|v| v.desc.flags.contains("U67")))?;
    if memory || useg {
        if !memory && !u67 {
            return Err("segment override requires address-size override support".into());
        }
        code.emit(
            "unsigned idx = UNLIKELY(flags & (FE_SEG_MASK|FE_ADDR32)) ? enc_seg67(buf, flags) : 0;",
            "let mut idx: usize = if flags & (FE_SEG_MASK|FE_ADDR32) != 0 { R::enc_seg67(&mut buf[..], flags) } else { 0 };",
        );
    } else if u67 {
        code.emit(
            "unsigned idx = UNLIKELY(flags & FE_ADDR32) ? (*buf=0x67, 1) : 0;",
            "let mut idx: usize = if flags & FE_ADDR32 != 0 { buf[0] = 0x67; 1 } else { 0 };",
        );
    } else {
        code.emit("(void) flags;", "let _ = flags;");
    }
    let has_idx = memory || useg || u67;
    let mut unconditional = false;
    for (i, variant) in variants.iter().enumerate() {
        let opcode = &variant.opcode;
        let desc = &variant.desc;
        let enc = encoding(&desc.encoding)?;
        let mut conds = Vec::new();
        if desc.encoding == "S" {
            let value = (opcode.opc >> 3) & 7;
            conds.push(Expr {
                c: format!("op_reg_idx(op0)=={value:#x}"),
                rust: format!("op0.idx=={value:#x}"),
            });
        }
        if desc.mnemonic == "XCHG_NOP" && opsize == 32 {
            conds.push(Expr {
                c: "!(op_reg_idx(op0)==0&&op_reg_idx(op1)==0)".into(),
                rust: "!(op0.idx==0&&op1.idx==0)".into(),
            });
        }
        if enc.vexreg_idx != 0 && opcode.vex == 0 {
            let op = enc.vexreg_idx ^ 3;
            conds.push(Expr {
                c: format!("op_reg_idx(op{op})=={}", enc.zeroreg_val),
                rust: format!("op{op}.idx=={}", enc.zeroreg_val),
            });
        }
        let imm_size = desc.imm_size(opsize / 8)?;
        let mut immsz = Expr::same(imm_size.to_string());
        let op = enc.imm_idx ^ 3;
        let mut imm = Expr {
            c: format!("(int64_t) op{op}"),
            rust: format!("(op{op} as i64)"),
        };
        match enc.imm_control {
            1 => conds.push(Expr::same(format!("op{op} == 1"))),
            2 => {
                immsz = Expr {
                    c: "(flags & FE_ADDR32 ? 4 : 8)".into(),
                    rust: "(if flags & FE_ADDR32 != 0 { 4 } else { 8 })".into(),
                };
                imm = Expr {
                    c: format!(
                        "(int64_t) (flags & FE_ADDR32 ? (int32_t) {} : {})",
                        imm.c, imm.c
                    ),
                    rust: format!(
                        "(if flags & FE_ADDR32 != 0 {{ op{op} as i32 as i64 }} else {{ op{op} as i64 }})"
                    ),
                };
            }
            3 => {
                imm = Expr {
                    c: format!("op_reg_idx(op{op}) << 4"),
                    rust: format!("(i64::from(op{op}.idx) << 4)"),
                };
                code.emit(
                    format!("if (op_reg_idx(op{op}) >= 16) return 0;"),
                    format!("if op{op}.idx >= 16 {{ return 0; }}"),
                );
            }
            4 if imm_size == 3 => {
                code.emit(
                    format!("if ((uint32_t) op{op} >= 0x1000000) return 0;"),
                    format!("if op{op} as u32 >= 0x1000000 {{ return 0; }}"),
                );
            }
            4 if imm_size < max_imm_size => {
                conds.push(Expr {
                    c: format!("op_imm_n({}, {imm_size})", imm.c),
                    rust: format!("R::op_imm_n({}, {imm_size})", imm.rust),
                });
            }
            6 => {
                imm.c = format!("{} - (int64_t) buf - {imm_size}", imm.c);
                imm.rust = format!(
                    "(op{op} as i64).wrapping_sub(address as i64).wrapping_sub({imm_size})"
                );
                if i != variants.len() - 1 {
                    conds.push(Expr {
                        c: "!(flags & FE_JMPL)".into(),
                        rust: "flags & FE_JMPL == 0".into(),
                    });
                    conds.push(Expr {
                        c: format!("op_imm_n({}-1, {imm_size})", imm.c),
                        rust: format!("R::op_imm_n(({}).wrapping_sub(1), {imm_size})", imm.rust),
                    });
                }
            }
            _ => {}
        }
        if !conds.is_empty() {
            code.emit(
                format!(
                    "if ({}) {{",
                    conds
                        .iter()
                        .map(|c| c.c.as_str())
                        .collect::<Vec<_>>()
                        .join("&&")
                ),
                format!(
                    "if {} {{",
                    conds
                        .iter()
                        .map(|c| format!("({})", c.rust))
                        .collect::<Vec<_>>()
                        .join(" && ")
                ),
            );
        }
        if opcode.vex != 0 {
            encode2_gen_vex(&mut code, variant, &imm, &immsz, has_idx)?;
        } else {
            encode2_gen_legacy(
                &mut code, variant, opsize, &high_regs, &imm, &immsz, has_idx,
            )?;
        }
        if !conds.is_empty() {
            code.emit("}", "}");
        } else {
            unconditional = true;
            break;
        }
    }
    if !unconditional {
        code.emit("return 0;", "return 0;");
    }
    code.emit("}", "}");
    writeln!(output.semantic_tsv, "function\t{fnname}\t{}", code.trace).unwrap();
    if !high_regs.is_empty() {
        let mut trace = format!("#define fe64_{}(buf, flags", mnem.name);
        for i in 0..op_tys.len() {
            write!(trace, ", op{i}").unwrap();
        }
        write!(trace, ") {fnname}(buf, flags").unwrap();
        for i in 0..op_tys.len() {
            if high_regs.contains(&i) {
                write!(trace, ", FE_MAKE_GPLH(op{i})").unwrap();
            } else {
                write!(trace, ", op{i}").unwrap();
            }
        }
        trace.push(')');
        let trace = trace
            .chars()
            .filter(|c| !c.is_ascii_whitespace())
            .collect::<String>();
        writeln!(output.semantic_tsv, "macro\t{fnname}\t{trace}").unwrap();
    }
    output.private_rs.push_str(&code.rust);
    Ok(())
}

const RUNTIME_CONTRACT: &str = r#"use crate::runtime::*;

pub trait Encode2Runtime {
    fn op_imm_n(imm: i64, size: usize) -> bool;
    fn enc_seg67(buf: &mut [u8], flags: u32) -> usize;
    fn enc_rex_mem(mem: FeMem, reg: u32) -> u32;
    fn enc_imm(buf: &mut [u8], imm: i64, size: usize);
    fn enc_mem(buf: &mut [u8], ripoff: usize, mem: FeMem, reg: u32, forcesib: u32, disp8scale: i32) -> usize;
    fn enc_vex_common(buf: &mut [u8], opcode: u32, base: u32, idx: u32, reg: u32, vvvv: u32) -> usize;
    fn enc_vex_reg(buf: &mut [u8], opcode: u32, rm: u32, reg: u32, vvvv: u32) -> usize;
    fn enc_evex_reg(buf: &mut [u8], opcode: u32, rm: u32, reg: u32, vvvv: u32) -> usize;
    fn enc_evex_xmm(buf: &mut [u8], opcode: u32, rm: u32, reg: u32, vvvv: u32) -> usize;
    fn enc_vex_mem(buf: &mut [u8], opcode: u32, rm: FeMem, reg: u32, vvvv: u32, ripoff: usize, forcesib: u32, disp8scale: i32) -> usize;
    fn enc_evex_mem(buf: &mut [u8], opcode: u32, rm: FeMem, reg: u32, vvvv: u32, ripoff: usize, forcesib: u32, disp8scale: i32) -> usize;
    fn enc_vex_vsib(buf: &mut [u8], opcode: u32, rm: FeMemV, reg: u32, vvvv: u32, ripoff: usize, forcesib: u32, disp8scale: i32) -> usize;
    fn enc_evex_vsib(buf: &mut [u8], opcode: u32, rm: FeMemV, reg: u32, vvvv: u32, ripoff: usize, forcesib: u32, disp8scale: i32) -> usize;
}
"#;

pub fn generate(entries: &[Entry]) -> Result<Encode2Output> {
    let mut output = Encode2Output {
        public_rs: RUNTIME_CONTRACT.into(),
        private_rs: String::new(),
        semantic_tsv: String::new(),
    };
    for mnemonic in encode_mnems(entries)? {
        emit_mnemonic(&mnemonic, &mut output)?;
    }
    Ok(output)
}

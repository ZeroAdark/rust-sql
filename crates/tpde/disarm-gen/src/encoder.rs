// SPDX-FileCopyrightText: 2024 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

use crate::expression::{identifier, integer_with_context, CType, Context};
use crate::parser::{Desc, Fmt, MaskedVal};
use crate::{check_feature, Group};

struct Function {
    types: &'static [&'static str],
    expansions: &'static [&'static str],
    single: bool,
    overwrite: bool,
    condition: Option<&'static str>,
}

fn function(name: &str) -> Result<Function, String> {
    macro_rules! f {
        ($types:expr,$exp:expr) => {
            Function {
                types: $types,
                expansions: $exp,
                single: false,
                overwrite: false,
                condition: None,
            }
        };
        ($types:expr,$exp:expr,$cond:expr) => {
            Function {
                condition: Some($cond),
                ..f!($types, $exp)
            }
        };
    }
    Ok(match name {
        "const" => f!(&[], &["{val}"]),
        "bool" => f!(&["bool"], &["!!{0}"]),
        "regzr" => f!(&[], &["31"]),
        "reggp" => f!(&["DA_GRegZR"], &["DA_REGVAL({0})"]),
        "reggpsp" => f!(&["DA_GRegSP"], &["DA_REGVAL({0})"]),
        "reggpnozr" => f!(&["DA_GReg"], &["DA_REGVAL({0})"]),
        "reggpls64" => f!(
            &["DA_GReg"],
            &["DA_REGVAL({0})"],
            "DA_REGVAL({0})<24&&!(DA_REGVAL({0})&1)"
        ),
        "reggpeven" => f!(&["DA_GReg"], &["DA_REGVAL({0})"], "!(DA_REGVAL({0})&1)"),
        "regfp" => f!(&["DA_VReg"], &["DA_REGVAL({0})"]),
        "regfplim" => Function {
            single: true,
            ..f!(
                &["DA_VReg"],
                &["DA_REGVAL({0})<<16"],
                "DA_REGVAL({0})<{max}"
            )
        },
        "prfop" => f!(&["enum Da64PrfOp  prfop"], &["{0}&31"]),
        "cond" => f!(&["Da64Cond"], &["{0}"]),
        "invcond" => f!(&["Da64Cond"], &["({0}^1)"]),
        "imm" => f!(&["unsigned"], &["{0}"], "{0}<{max}"),
        "immadr" => f!(
            &["uintptr_t  codeaddr", "uintptr_t  target"],
            &["({1}-{0})&3", "(({1}-{0})>>2)&0x7ffff"],
            "da_sext(({1}-{0}),21)==(intptr_t)({1}-{0})"
        ),
        "immadrp" => f!(
            &["uintptr_t  codeaddr", "uintptr_t  target"],
            &[
                "((({1}&~0xfffull)-({0}&~0xfffull))>>12)&3",
                "((({1}&~0xfffull)-({0}&~0xfffull))>>14)&0x7ffff"
            ],
            "da_sext(({1}&~0xfffull)-({0}&~0xfffull),33)==(intptr_t)(({1}&~0xfffull)-({0}&~0xfffull))"
        ),
        "immlsl" => f!(
            &["unsigned  lsl"],
            &["(-{0})&({max}-1)", "{max}-1-{0}"],
            "{0}<{max}"
        ),
        "immbfx" => f!(
            &["unsigned  lsb", "unsigned  width"],
            &["{0}", "{0}+{1}-1"],
            "{0}<{max}&&{1}-1<{max}-{0}"
        ),
        "immbfi" => f!(
            &["unsigned  lsb", "unsigned  width"],
            &["(-{0})&({max}-1)", "{1}-1"],
            "{0}<{max}&&{1}-1<{max}-{0}"
        ),
        "immshiftr" => Function {
            single: true,
            ..f!(
                &["unsigned  imm"],
                &["({base}-{0})<<16"],
                "{0}&&{0}<={base}"
            )
        },
        "immshiftl" => Function {
            single: true,
            ..f!(&["unsigned  imm"], &["{0}<<16"], "{0}&&{0}<={base}")
        },
        "immldraut" => f!(
            &["int64_t"],
            &["({0}>>12)&1", "({0}&0xff8)>>3"],
            "da_sext({0},13)=={0}&&({0}&7)==0"
        ),
        "immvidx" => Function {
            overwrite: true,
            ..f!(
                &["unsigned"],
                &["{0}<<({size}+{shift})"],
                "{0}<(1u<<(4-{size}))"
            )
        },
        "immrotmul" => f!(&["unsigned"], &["{0}/90"], "{0}<360&&{0}%90==0"),
        "immrotadd" => f!(&["unsigned"], &["{0}/90-1"], "{0}==90||{0}==270"),
        "velemidx" => Function {
            overwrite: true,
            ..f!(
                &["unsigned  elemidx"],
                &["({0}<<{size})>>3", "({0}<<{size})>>2", "({0}<<{size})>>1"],
                "{0}<(1u<<(4-{size}))"
            )
        },
        "velemidxlim" => Function {
            overwrite: true,
            ..f!(
                &["unsigned  elemidx"],
                &["({0}<<{size})>>3", "({0}<<{size})>>2", "({0}<<{size})>>1"],
                "{0}<(1u<<(4-{size}))&&{0}<{lim}"
            )
        },
        "velemidx0" => Function {
            overwrite: true,
            ..f!(
                &["unsigned  elemidx"],
                &[
                    "({0}<<{size})>>3",
                    "({0}<<{size})>>2",
                    "({0}<<{size})>>1",
                    "(({0}<<{size})&1)<<3"
                ],
                "{0}<(1u<<(4-{size}))"
            )
        },
        "memsimdidx" => Function {
            overwrite: true,
            ..f!(
                &["unsigned  elemidx"],
                &["({0}<<{size})>>3", "({0}<<{size})>>2", "{0}<<{size}"],
                "{0}<(1u<<(4-{size}))"
            )
        },
        "uimm" => f!(
            &["uint64_t"],
            &["{0}>>{lsr}"],
            "({0}&(((1<<{bits})-1)<<{lsr}))=={0}"
        ),
        "simm" => f!(
            &["int64_t"],
            &["{0}/(1<<{asr})"],
            "da_sext({0},{bits}+{asr})=={0}&&({0}&((1<<{asr})-1))==0"
        ),
        "tbz" => f!(&["unsigned  bit"], &["{0}>>5", "{0}&0x1f"], "{0}<64"),
        "reladdr" => f!(&["ptrdiff_t"], &["{0}"], "da_sext({0},{bits})=={0}"),
        "fcvtfixscale" => f!(&["unsigned  fbits"], &["64-{0}"], "{0}<{max}"),
        "immadd" => Function {
            single: true,
            ..f!(
                &["int{size}_t  imm"],
                &["da_immadd({0})"],
                "da_immadd({0}) != 0xffffffff"
            )
        },
        "immlogical" => Function {
            single: true,
            ..f!(
                &["uint{size}_t  imm"],
                &["da_immlogical({0}, {size}>>6)"],
                "da_immlogical({0}, {size}>>6) != 0xffffffff"
            )
        },
        "immfmov32" => f!(
            &["float  imm"],
            &["da_immfmov32({0})"],
            "da_immfmov32({0}) != 0xffffffff"
        ),
        "immfmov64" => f!(
            &["double  imm"],
            &["da_immfmov64({0})"],
            "da_immfmov64({0}) != 0xffffffff"
        ),
        "immsimd8movi" => Function {
            single: true,
            ..f!(
                &["uint64_t  imm64"],
                &["da_immsimdmovi({0})"],
                "da_immsimdmovi({0}) != 0xffffffff"
            )
        },
        "immsimd8lsl" => f!(
            &["uint8_t  imm", "unsigned  lsl"],
            &["{0}>>5", "{0}&0x1f", "{1}>>2|1"],
            "{1}<(8*{maxshift}) && ({1}&7) == 0"
        ),
        "immsimd8fmov" => f!(
            &["float  imm"],
            &["da_immfmov32({0})>>5", "da_immfmov32({0})&0x1f"],
            "da_immfmov32({0}) != 0xffffffff"
        ),
        _ => return Err(format!("unknown encoder operand function {name}")),
    })
}

fn substitute(
    template: &str,
    args: &[String],
    options: &[(String, String)],
) -> Result<String, String> {
    let mut result = String::new();
    let mut remainder = template;
    while let Some(pos) = remainder.find('{') {
        result.push_str(&remainder[..pos]);
        remainder = &remainder[pos + 1..];
        let end = remainder.find('}').ok_or("unclosed operand placeholder")?;
        let key = &remainder[..end];
        let value = if let Ok(index) = key.parse::<usize>() {
            args.get(index).map(String::as_str)
        } else {
            options
                .iter()
                .rev()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        result.push_str(value.ok_or_else(|| format!("missing operand option {key}"))?);
        remainder = &remainder[end + 1..];
    }
    result.push_str(remainder);
    Ok(result)
}

fn assign(
    assignments: &mut Vec<(String, String)>,
    key: &str,
    value: String,
    overwrite: bool,
) -> Result<(), String> {
    if let Some((_, prior)) = assignments.iter_mut().find(|(k, _)| k == key) {
        if !overwrite {
            return Err(format!("duplicate assignment to {key}"));
        }
        *prior = value;
    } else {
        assignments.push((key.into(), value));
    }
    Ok(())
}

fn rust_type(c: &str) -> Result<&'static str, String> {
    Ok(match c {
        "DA_GReg" => "GReg",
        "DA_GRegZR" => "GRegZr",
        "DA_GRegSP" => "GRegSp",
        "DA_VReg" => "VReg",
        "enum Da64PrfOp" => "PrfOp",
        "Da64Cond" => "Cond",
        "bool" => "bool",
        "unsigned" | "uint32_t" => "u32",
        "uint8_t" => "u8",
        "uint64_t" => "u64",
        "int32_t" => "i32",
        "int64_t" => "i64",
        "uintptr_t" => "usize",
        "ptrdiff_t" => "isize",
        "float" => "f32",
        "double" => "f64",
        _ => return Err(format!("unknown encoder parameter type {c}")),
    })
}

#[derive(Default)]
pub struct Encoder {
    inline: bool,
    decls: Vec<String>,
    definitions: Vec<String>,
    rust_functions: Vec<String>,
    manifest: String,
    names: Vec<String>,
}

pub struct EncoderOutput {
    pub public: String,
    pub private: String,
    pub manifest: String,
    pub public_c: String,
    pub private_c: Vec<(String, String)>,
}

impl Encoder {
    pub fn new(inline: bool) -> Self {
        Self {
            inline,
            ..Self::default()
        }
    }

    pub fn add_group(&mut self, group: &Group, features: &[(String, bool)]) -> Result<(), String> {
        Fmt::parse(group.get("feat").unwrap_or(""), &group.desc)?;
        for (key, value) in &group.props {
            let Some(category) = key.strip_prefix("encmnem") else {
                continue;
            };
            let mnemonic = Fmt::parse(value, &group.desc)?;
            let operands = Fmt::parse(
                group
                    .get(&format!("encops{category}"))
                    .ok_or_else(|| format!("{} missing encops{category}", group.name))?,
                &group.desc,
            )?;
            let condition = Fmt::parse(
                group.get(&format!("enccond{category}")).unwrap_or(""),
                &group.desc,
            )?;
            let feature = Fmt::parse(
                group
                    .get(&format!("encfeat{category}"))
                    .or_else(|| group.get("feat"))
                    .unwrap_or(""),
                &group.desc,
            )?;
            for (name, masks) in mnemonic.expansions()? {
                if masks.len() != 1 {
                    return Err(format!("{name}: encmnem with multiple descs per name"));
                }
                let ops = operands.eval(&masks)?;
                let cond = condition.eval(&masks)?;
                let feat = feature
                    .eval(&masks)?
                    .ok_or_else(|| format!("{name}: undefined feature"))?;
                let enabled = check_feature(features, &feat)?;
                self.manifest.push_str(&format!(
                    "form\t{}\t{category}\t{name}\t{:#x}\t{:#x}\t{}\t{feat}\t{}\t{}\n",
                    group.name,
                    masks[0].val,
                    masks[0].mask,
                    enabled as u8,
                    ops.as_deref().unwrap_or("None"),
                    cond.as_deref().unwrap_or("None")
                ));
                if enabled {
                    self.add_mnem(
                        &group.name,
                        &name,
                        &group.desc,
                        masks[0],
                        ops.as_deref()
                            .ok_or_else(|| format!("{name}: undefined operands"))?,
                        cond.as_deref().unwrap_or(""),
                    )?;
                }
            }
        }
        Ok(())
    }

    pub fn add_mnem(
        &mut self,
        group: &str,
        name: &str,
        desc: &Desc,
        fixed: MaskedVal,
        ops: &str,
        cond: &str,
    ) -> Result<(), String> {
        let mut paramtypes: Vec<String> = vec![];
        let mut names: Vec<String> = vec![];
        let mut conditions = vec![];
        let mut assignments = vec![];
        let mut grand = vec![];
        let mut aliases = vec![];
        for component in ops.split_whitespace() {
            let (variables, spec) = component
                .split_once('=')
                .ok_or_else(|| format!("invalid encoder operand {component}"))?;
            let variables: Vec<&str> = variables.split(',').collect();
            if let Some(spec) = spec.strip_prefix('@') {
                let targets: Vec<&str> = spec.split(',').collect();
                if variables.len() != targets.len() {
                    return Err("var alias mismatch".into());
                }
                for (alias, target) in variables.iter().zip(targets) {
                    assign(&mut aliases, alias, target.to_owned(), true)?;
                }
                continue;
            }
            let (funcname, options) = spec.split_once('/').unwrap_or((spec, ""));
            let options: Vec<(String, String)> = options
                .split(',')
                .filter(|s| !s.is_empty())
                .map(|s| {
                    let (k, v) = s.split_once('=').unwrap_or((s, ""));
                    (k.into(), v.into())
                })
                .collect();
            let func = function(funcname)?;
            let basename = variables.concat();
            let mut funcnames = vec![];
            for spec in func.types {
                let (ty, explicit) = spec.split_once("  ").unwrap_or((spec, ""));
                funcnames.push(if explicit.is_empty() {
                    basename.clone()
                } else {
                    explicit.into()
                });
                paramtypes.push(substitute(ty, &[], &options)?);
            }
            names.extend(funcnames.iter().cloned());
            if let Some(condition) = func.condition {
                conditions.push(substitute(condition, &funcnames, &options)?);
            }
            if func.single {
                grand.push(substitute(func.expansions[0], &funcnames, &options)?);
                for var in variables {
                    assign(&mut assignments, var, "0".into(), true)?;
                }
                continue;
            }
            if variables.len() != func.expansions.len() {
                return Err(format!("{name}: operand expansion count mismatch"));
            }
            for (var, exp) in variables.iter().zip(func.expansions) {
                assign(
                    &mut assignments,
                    var,
                    substitute(exp, &funcnames, &options)?,
                    func.overwrite,
                )?;
            }
        }
        for (alias, var) in aliases {
            let value = assignments
                .iter()
                .find(|(k, _)| k == &var)
                .ok_or_else(|| format!("unknown alias source {var}"))?
                .1
                .clone();
            assign(&mut assignments, &alias, value, true)?;
        }
        if !cond.is_empty() {
            let mut remainder = cond;
            let mut expanded = String::new();
            while let Some(pos) = remainder.find('@') {
                expanded.push_str(&remainder[..pos]);
                remainder = &remainder[pos + 1..];
                let len = remainder
                    .bytes()
                    .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
                    .count();
                let key = &remainder[..len];
                let value = &assignments
                    .iter()
                    .find(|(k, _)| k == key)
                    .ok_or_else(|| format!("unknown condition variable {key}"))?
                    .1;
                expanded.push('(');
                expanded.push_str(value);
                expanded.push(')');
                remainder = &remainder[len..];
            }
            expanded.push_str(remainder);
            conditions.push(expanded);
        }
        let mut xor = vec![format!("{:#x}", fixed.val)];
        xor.extend(grand);
        let mut expressions = vec![format!("({})", xor.join("^"))];
        for entry in desc {
            let mask = if entry.size == 32 {
                u32::MAX
            } else {
                (1u32 << entry.size) - 1
            };
            if let Some((_, assignment)) = assignments
                .iter()
                .find(|(k, _)| Some(k.as_str()) == entry.name.as_deref())
            {
                expressions.push(format!("((({assignment})&{mask:#x})<<{})", entry.idx));
            } else if fixed.mask & (mask << entry.idx) != mask << entry.idx {
                self.manifest.push_str(&format!(
                    "omitted\t{group}\t{name}\t{}\n",
                    entry.name.as_deref().unwrap_or("")
                ));
                return Ok(());
            }
        }
        if self.names.iter().any(|n| n == name) {
            return Err(format!("duplicate encoder name {name}"));
        }
        self.names.push(name.into());
        let expression = expressions.join("|");
        let full_expression = if conditions.is_empty() {
            expression.clone()
        } else {
            format!(
                "!({}) ? 0 : {expression}",
                conditions
                    .iter()
                    .map(|s| format!("({s})"))
                    .collect::<Vec<_>>()
                    .join("&&")
            )
        };
        let needs_macro = paramtypes
            .iter()
            .any(|t| t == "DA_GRegSP" || t == "DA_GRegZR");
        let implname = format!("de64_{}{name}", if needs_macro { "impl_" } else { "" });
        if needs_macro {
            let args = paramtypes
                .iter()
                .zip(&names)
                .map(|(ty, name)| {
                    format!(
                        "{}({name})",
                        match ty.as_str() {
                            "DA_GRegSP" => "DA_GPSP",
                            "DA_GRegZR" => "DA_GPZR",
                            _ => "",
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            self.decls.push(format!(
                "#define de64_{name}({}) {implname}({args})",
                names.join(",")
            ));
        }
        let parameters = paramtypes
            .iter()
            .zip(&names)
            .map(|(ty, name)| format!("{ty} {name}"))
            .collect::<Vec<_>>()
            .join(", ");
        let signature = format!(
            "uint32_t {implname}({})",
            if parameters.is_empty() {
                "void"
            } else {
                &parameters
            }
        );
        if self.inline {
            self.decls.push(format!(
                "static inline {signature} {{ return {full_expression}; }}"
            ));
        } else {
            self.decls.push(format!("{signature};"));
            self.definitions
                .push(format!("{signature} {{ return {full_expression}; }}"));
        }
        let mut context = Context::default();
        for (ty, name) in paramtypes.iter().zip(&names) {
            let cty = if ty.starts_with("DA_") {
                CType::Register
            } else if ty == "Da64Cond" || ty == "enum Da64PrfOp" {
                CType::Unsigned(32)
            } else {
                CType::from_c(ty)?
            };
            context.variables.insert(name.clone(), cty);
        }
        let parameters = paramtypes
            .iter()
            .zip(&names)
            .map(|(ty, name)| Ok(format!("{}: {}", identifier(name), rust_type(ty)?)))
            .collect::<Result<Vec<_>, String>>()?
            .join(", ");
        let rust_expr = integer_with_context(&full_expression, &context)
            .map_err(|e| format!("{group}/{name}: {e}"))?;
        self.rust_functions.push(format!(
            "{}pub fn de64_{name}({parameters}) -> u32 {{ ({rust_expr}) as u32 }}\n",
            if self.inline { "#[inline]\n" } else { "" }
        ));
        self.manifest.push_str(&format!(
            "emitted\t{group}\t{name}\t{}\n",
            self.names.len() - 1
        ));
        Ok(())
    }

    pub fn generate(self) -> Result<EncoderOutput, String> {
        let module = format!(
            "#[allow(non_snake_case,unused_imports,unused_parens)]\nmod __disarm_encoder {{\nuse super::*;\nuse super::super::runtime::*;\n{}\n}}\n",
            self.rust_functions.concat()
        );
        Ok(EncoderOutput {
            public: format!(
                "pub use self::__disarm_encoder::*;\n{}",
                if self.inline { module.as_str() } else { "" }
            ),
            private: if self.inline { String::new() } else { module },
            manifest: self.manifest,
            public_c: self.decls.join("\n") + "\n",
            private_c: vec![("DA64_ENCODER".into(), self.definitions.join("\n"))],
        })
    }
}

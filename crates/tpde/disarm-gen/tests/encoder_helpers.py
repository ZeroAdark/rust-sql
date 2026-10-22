#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2024 Alexis Engelke
# SPDX-License-Identifier: BSD-3-Clause

import argparse
import json
from pathlib import Path
import re
import runpy
import struct

import encoder_cases as base
import oracle


SCALARS = {
    **base.SCALAR_TYPES, "int32_t": "i32", "int64_t": "i64",
    "uintptr_t": "usize", "ptrdiff_t": "isize", "float": "f32", "double": "f64",
}
MASK = base.U64_MAX
MIX_A = 0xFF51AFD7ED558CCD
MIX_B = 0xC4CEB9FE1A85EC53
MIX_ARG = 0x9E3779B97F4A7C15
MIX_TAG = 0x100000001B3


def fingerprint(tag, first, second=0):
    value = (first ^ ((second * MIX_ARG) & MASK) ^ ((tag * MIX_TAG) & MASK)) & MASK
    value ^= value >> 33
    value = (value * MIX_A) & MASK
    value ^= value >> 33
    value = (value * MIX_B) & MASK
    value ^= value >> 33
    return base.U32_MAX if value & 7 == 0 else (value ^ (value >> 32)) & base.U32_MAX


def contract_result(family, options, value):
    if family == "immadd":
        width = int(options["size"], 0)
        value &= (1 << width) - 1
        signed = value - (1 << width) if value & (1 << (width - 1)) else value
        return fingerprint(1, signed & MASK)
    if family == "immlogical":
        width = int(options["size"], 0)
        return fingerprint(2, value & ((1 << width) - 1), width >> 6)
    if family in ["immfmov32", "immsimd8fmov"]:
        return fingerprint(3, value & base.U32_MAX)
    if family == "immfmov64":
        return fingerprint(4, value)
    if family == "immsimd8movi":
        return fingerprint(5, value)
    return None


def operand_domain(family, options, ty, part):
    if ty == "float":
        values = [0, 0x80000000, 0x3F800000, 0xBF800000, 0x3F000000, 0x3FC00000,
                  0x40000000, 0x7F800000, 0xFF800000, 0x7FC00001, 0x7F800001,
                  0x007FFFFF, 0x00800000, 1, 0xFFFFFFFF, 0x12345678]
    elif ty == "double":
        values = [0, 0x8000000000000000, 0x3FF0000000000000, 0xBFF0000000000000,
                  0x3FE0000000000000, 0x3FF8000000000000, 0x4000000000000000,
                  0x7FF0000000000000, 0xFFF0000000000000, 0x7FF8000000000001,
                  0x7FF0000000000001, 0x000FFFFFFFFFFFFF, 0x0010000000000000,
                  1, MASK, 0x123456789ABCDEF0]
    elif ty in ["int32_t", "int64_t", "ptrdiff_t"]:
        values = [0, 1, -1, 2, -2, 7, -7, 8, -8, 4095, 4096, 4097, -4095, -4096, -4097,
                  (1 << 31) - 1, 1 << 31, -(1 << 31), -(1 << 31) - 1,
                  (1 << 63) - 1, -(1 << 63), MASK]
        if family in ["simm", "reladdr", "immldraut"]:
            bits = 13 if family == "immldraut" else int(options["bits"], 0)
            shift = 3 if family == "immldraut" else int(options.get("asr", "0"), 0)
            width = bits if family == "immldraut" else bits + shift
            high, low, quantum = (1 << (width - 1)) - 1, -(1 << (width - 1)), 1 << shift
            aligned_high = high // quantum * quantum
            values += [low - quantum, low - 1, low, low + 1, low + quantum,
                       high - quantum, high - 1, high, high + 1, high + quantum,
                       aligned_high - quantum, aligned_high, aligned_high + quantum,
                       quantum - 1, quantum, quantum + 1, -quantum - 1, -quantum, -quantum + 1]
    elif ty == "uintptr_t":
        values = [0, 1, 0xFFF, 0x1000, 0x1001, 0x1FFFFF, 0x200000,
                  (1 << 32) - 1, 1 << 32, (1 << 48) - 1, 1 << 48,
                  (1 << 63) - 1, 1 << 63, MASK - 0xFFF, MASK]
    else:
        values = base.domain(family, options, ty, part)
    values = [value & MASK for value in values]
    if contract_result(family, options, 0) is not None:
        state = 0x0123456789ABCDEF
        for _ in range(64):
            state ^= (state << 13) & MASK
            state ^= state >> 7
            state ^= (state << 17) & MASK
            values.append(state & base.U32_MAX if ty in ["float", "uint32_t", "int32_t"] else state)
        outcomes = [contract_result(family, options, value) for value in values]
        assert any(value == base.U32_MAX for value in outcomes), (family, options, "missing rejection")
        assert any(value != base.U32_MAX for value in outcomes), (family, options, "missing acceptance")
    return base.unique(values)


def parameters(form, functions):
    params, groups = [], []
    for operand in form["operands"].split():
        variables, spec = operand.split("=", 1)
        if spec.startswith("@"):
            continue
        family, _, option_text = spec.partition("/")
        options = dict(option.split("=", 1) for option in option_text.split(",") if option)
        first = len(params)
        for part, parameter in enumerate(functions[family].paramtys):
            ty, _, name = parameter.partition("  ")
            ty = ty.format(**options)
            assert ty in base.REGISTER_TYPES or ty in SCALARS, (form["name"], family, ty)
            params.append({
                "name": name or variables.replace(",", ""), "type": ty,
                "family": family, "options": options, "part": part,
                "domain": operand_domain(family, options, ty, part),
            })
        if first != len(params):
            groups.append((family, options, list(range(first, len(params)))))
    return params, groups


def cases(params, groups, seed):
    result = dict(base.cases(params, groups, seed))
    baseline = list(next(iter(result)))
    for family, _, indices in groups:
        if family not in ["immadr", "immadrp"]:
            continue
        limit = 1 << (32 if family == "immadrp" else 20)
        deltas = [-limit - 4096, -limit - 1, -limit, -limit + 1, -4097, -4096,
                  -1, 0, 1, 4095, 4096, 4097, limit - 1, limit, limit + 1, limit + 4096]
        for origin in [0, 0xFFF, 0x1000, 0x1001, 0x100001001, MASK - 0xFFF]:
            for delta in deltas:
                values = baseline.copy()
                values[indices[0]], values[indices[1]] = origin, (origin + delta) & MASK
                result.setdefault(tuple(values), f"address:{family}:{origin:#x}:{delta}")
    return result.items()


def rust_argument(param, index):
    ty = param["type"]
    if ty in base.REGISTER_TYPES:
        return base.rust_argument(param, index)
    if ty == "float":
        return f"f32::from_bits(args[{index}] as u32)"
    if ty == "double":
        return f"f64::from_bits(args[{index}])"
    if ty == "bool":
        return f"(args[{index}] != 0)"
    return f"(args[{index}] as {SCALARS[ty]})"


def cpp_argument(param, index):
    if param["type"] == "float":
        return f"input_f32(args[{index}])"
    if param["type"] == "double":
        return f"input_f64(args[{index}])"
    return base.cpp_argument(param, index)


def runtime_sources():
    rust = f"""
mod runtime {{
    #[derive(Clone, Copy)] pub struct GReg(pub u8);
    #[derive(Clone, Copy)] pub struct GRegZr(pub u8);
    #[derive(Clone, Copy)] pub struct GRegSp(pub u8);
    #[derive(Clone, Copy)] pub struct VReg(pub u8);
    impl GReg {{ pub fn value(self)->u8 {{ self.0 }} }}
    impl GRegZr {{ pub fn value(self)->u8 {{ self.0 }} }}
    impl GRegSp {{ pub fn value(self)->u8 {{ self.0 }} }}
    impl VReg {{ pub fn value(self)->u8 {{ self.0 }} }}
    pub type Cond = u32;
    pub type PrfOp = u32;
    fn fingerprint(tag:u64, first:u64, second:u64)->u32 {{
        let mut value = first ^ second.wrapping_mul({MIX_ARG}u64) ^ tag.wrapping_mul({MIX_TAG}u64);
        value ^= value >> 33;
        value = value.wrapping_mul({MIX_A}u64);
        value ^= value >> 33;
        value = value.wrapping_mul({MIX_B}u64);
        value ^= value >> 33;
        if value & 7 == 0 {{ u32::MAX }} else {{ (value ^ (value >> 32)) as u32 }}
    }}
    pub fn da_sext(imm:i64, bits:u32)->i64 {{
        let mask = (1u64 << bits) - 1;
        let sign = 1u64 << (bits - 1);
        (((imm as u64 & mask) ^ sign).wrapping_sub(sign)) as i64
    }}
    pub fn da_immadd(value:i64)->u32 {{ fingerprint(1,value as u64,0) }}
    pub fn da_immlogical(value:u64,is64:u32)->u32 {{ fingerprint(2,value,is64 as u64) }}
    pub fn da_immfmov32(value:f32)->u32 {{ fingerprint(3,value.to_bits() as u64,0) }}
    pub fn da_immfmov64(value:f64)->u32 {{ fingerprint(4,value.to_bits(),0) }}
    pub fn da_immsimdmovi(value:u64)->u32 {{ fingerprint(5,value,0) }}
}}
"""
    cpp = f"""
static uint32_t fingerprint(uint64_t tag, uint64_t first, uint64_t second) {{
    uint64_t value = first ^ (second * UINT64_C({MIX_ARG})) ^ (tag * UINT64_C({MIX_TAG}));
    value ^= value >> 33;
    value *= UINT64_C({MIX_A});
    value ^= value >> 33;
    value *= UINT64_C({MIX_B});
    value ^= value >> 33;
    return (value & 7) == 0 ? UINT32_MAX : uint32_t(value ^ (value >> 32));
}}
uint32_t da_immadd(int64_t value) {{ return fingerprint(1,uint64_t(value),0); }}
uint32_t da_immlogical(uint64_t value, unsigned is64) {{ return fingerprint(2,value,is64); }}
uint32_t da_immfmov32(float value) {{
    uint32_t bits;
    std::memcpy(&bits,&value,sizeof(bits));
    return fingerprint(3,bits,0);
}}
uint32_t da_immfmov64(double value) {{
    uint64_t bits;
    std::memcpy(&bits,&value,sizeof(bits));
    return fingerprint(4,bits,0);
}}
uint32_t da_immsimdmovi(uint64_t value) {{ return fingerprint(5,value,0); }}
static float input_f32(uint64_t raw) {{
    uint32_t bits = uint32_t(raw);
    float value;
    std::memcpy(&value,&bits,sizeof(value));
    return value;
}}
static double input_f64(uint64_t bits) {{
    double value;
    std::memcpy(&value,&bits,sizeof(value));
    return value;
}}
"""
    return rust, cpp


def emit(args):
    oracle.verify_sources()
    oracle.generate(argparse.Namespace(
        output=args.output, features="really-all", coverage=False,
        encode_in_header=args.encode_in_header, no_feature_desc=False,
        feature_desc=None, desc=None, model=True, classifications=False,
    ))
    model = json.loads((args.output / "model.json").read_text())
    functions = runpy.run_path(str(oracle.REFERENCE / "parse.py"), run_name="__helper_cases__")["ENC_FUNCS"]
    forms = {form["name"]: form for form in model["encoder_forms"] if form["enabled"]}
    definitions = model["encoder_declarations"] if args.encode_in_header else model["encoder_definitions"]
    selected, helper_free, emitted = [], [], set()
    for definition in definitions:
        if args.encode_in_header and not definition.startswith("static inline uint32_t "):
            continue
        match = base.DEFINITION.fullmatch(definition)
        assert match, definition
        name = match[2]
        assert name in forms and name not in emitted, name
        emitted.add(name)
        helpers = sorted(set(re.findall(r"\bda_\w+(?=\s*\()", match[4])))
        if not helpers:
            helper_free.append(name)
            continue
        params, groups = parameters(forms[name], functions)
        signature = ", ".join(f"{param['type']} {param['name']}" for param in params) or "void"
        assert signature == match[3], (name, signature, match[3])
        selected.append((name, definition, params, groups, helpers))
    assert selected and helper_free
    maximum_args = max(len(params) for _, _, params, _, _ in selected)
    record_bytes = 4 + 8 * maximum_args
    rust_arms, cpp_arms, roster, corpus, labels, controls = [], [], [], bytearray(), [], []
    offset = 0
    for encoder_id, (name, _, params, groups, helpers) in enumerate(selected):
        rust_args = ", ".join(rust_argument(param, index) for index, param in enumerate(params))
        cpp_args = ", ".join(cpp_argument(param, index) for index, param in enumerate(params))
        rust_arms.append(f"{encoder_id} => generated::de64_{name}({rust_args}),")
        cpp_arms.append(f"case {encoder_id}: value = de64_{name}({cpp_args}); break;")
        first = offset
        for values, label in cases(params, groups, 0x9E3779B9 ^ (encoder_id + 1)):
            padded = list(values) + [0] * (maximum_args - len(values))
            corpus.extend(struct.pack(f"<I{maximum_args}Q", encoder_id, *padded))
            labels.append(f"{offset}\t{encoder_id}\t{name}\t{label}\t" + ",".join(map(str, values)) + "\n")
            offset += 1
        assert offset > first, name
        roster.append(f"{encoder_id}\t{name}\t{forms[name]['group']}\tSELECTED\t{first}\t{offset-first}\t{','.join(helpers)}\n")
        for param in params:
            outcomes = [(value, contract_result(param["family"], param["options"], value))
                        for value in param["domain"]]
            if outcomes[0][1] is not None:
                success = next(value for value, result in outcomes if result != base.U32_MAX)
                reject = next(value for value, result in outcomes if result == base.U32_MAX)
                controls.append(f"{name}\t{param['name']}\t{param['family']}\t{param['type']}\t{success:#x}\t{reject:#x}\n")
    for name in helper_free:
        roster.append(f"-\t{name}\t{forms[name]['group']}\tHELPER_FREE_SUITE\t-\t0\t\n")
    for name in sorted(forms.keys() - emitted):
        roster.append(f"-\t{name}\t{forms[name]['group']}\tUPSTREAM_OMITTED\t-\t0\t\n")
    rust_runtime, cpp_runtime = runtime_sources()
    rust_main = f"""
fn main() {{
    use std::io::{{Read, Write}};
    assert_eq!(std::mem::size_of::<usize>(),8);
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input).unwrap();
    assert_eq!(input.len() % {record_bytes},0);
    let mut output = Vec::with_capacity(input.len() / {record_bytes} * 4);
    for record in input.chunks_exact({record_bytes}) {{
        let id = u32::from_le_bytes(record[..4].try_into().unwrap());
        let mut args = [0u64; {maximum_args}];
        for (arg,bytes) in args.iter_mut().zip(record[4..].chunks_exact(8)) {{
            *arg = u64::from_le_bytes(bytes.try_into().unwrap());
        }}
        let value = match id {{
            {chr(10).join(rust_arms)}
            _ => panic!("unknown encoder {{id}}"),
        }};
        output.extend_from_slice(&value.to_le_bytes());
    }}
    std::io::stdout().write_all(&output).unwrap();
}}
"""
    cpp_definitions = "" if args.encode_in_header else "\n".join(item[1] for item in selected)
    cpp_source = f"""
#include <cstdio>
#include <cstring>
#include "disarm64.h"
static_assert(sizeof(uintptr_t)==8 && sizeof(ptrdiff_t)==8);
{cpp_runtime}
{cpp_definitions}
int main() {{
    unsigned char record[{record_bytes}];
    size_t size;
    while ((size = std::fread(record,1,sizeof(record),stdin)) != 0) {{
        if (size != sizeof(record)) return 2;
        uint32_t id = 0, value = 0;
        for (unsigned i=0;i<4;++i) id |= uint32_t(record[i]) << (8*i);
        uint64_t args[{maximum_args}] = {{}};
        for (unsigned i=0;i<{maximum_args};++i)
            for (unsigned j=0;j<8;++j)
                args[i] |= uint64_t(record[4+i*8+j]) << (8*j);
        switch (id) {{
            {chr(10).join(cpp_arms)}
            default: return 3;
        }}
        for (unsigned i=0;i<4;++i) record[i] = value >> (8*i);
        if (std::fwrite(record,1,4,stdout) != 4) return 4;
    }}
    return std::ferror(stdin) ? 5 : 0;
}}
"""
    (args.output / "disarm64.h").write_bytes((oracle.REFERENCE / "disarm64.h").read_bytes())
    (args.output / "disarm64-public.inc").write_bytes((args.output / "public.inc").read_bytes())
    (args.output / "helpers-runtime.rs").write_text(rust_runtime)
    (args.output / "helpers-main.rs").write_text(rust_main)
    (args.output / "helpers.cpp").write_text(cpp_source)
    (args.output / "helpers-input.bin").write_bytes(corpus)
    (args.output / "helpers-cases.tsv").write_text("case\tencoder\tname\tlabel\toperands\n" + "".join(labels))
    (args.output / "helpers-roster.tsv").write_text("encoder\tname\tgroup\tdisposition\tfirst_case\tcases\thelpers\n" + "".join(roster))
    (args.output / "helpers-controls.tsv").write_text("name\tparameter\tfamily\ttype\tsuccess_bits\treject_bits\n" + "".join(controls))
    (args.output / "helpers-contract.txt").write_text(
        "Scope: generated expressions calling helper contracts; no encode.c implementation claim.\n"
        "Source: Disarm 2d13d3f410a52daff1c5d8ef07d623332f372560 parse.py and disarm64.h.\n"
        "da_sext: pinned C inline function and equivalent test-only Rust sign extension.\n"
        "Immediate helpers: pure argument-sensitive fingerprints; low three hash bits zero returns 0xffffffff.\n"
        "Float arguments preserve raw bits with memcpy/from_bits; signed operands use two's-complement bits.\n"
        "helpers-controls.tsv records a success and rejection witness for each immediate-helper parameter.\n"
        "Helper-free generated definitions are covered by the separate encoder_generated target.\n"
    )
    (args.output / "helpers-counts.tsv").write_text(
        f"selected\t{len(selected)}\nhelper_free\t{len(helper_free)}\nomitted\t{len(forms)-len(emitted)}\n"
        f"cases\t{offset}\nrecord_bytes\t{record_bytes}\n"
    )


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("output", type=Path)
    parser.add_argument("--encode-in-header", action="store_true")
    emit(parser.parse_args())

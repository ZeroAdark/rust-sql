#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2024 Alexis Engelke
# SPDX-License-Identifier: BSD-3-Clause

import argparse
import itertools
import json
from pathlib import Path
import re
import runpy
import struct

import oracle


DEFINITION = re.compile(
    r"(?:static inline )?uint32_t (de64_(?:impl_)?(\w+))\((.*?)\) \{ return (.*); \}"
)
REGISTER_TYPES = {
    "DA_GReg": "GReg", "DA_GRegZR": "GRegZr",
    "DA_GRegSP": "GRegSp", "DA_VReg": "VReg",
}
SCALAR_TYPES = {
    "bool": "bool", "unsigned": "u32", "uint8_t": "u8",
    "uint32_t": "u32", "uint64_t": "u64",
    "Da64Cond": "u32", "enum Da64PrfOp": "u32",
}
U32_MAX = (1 << 32) - 1
U64_MAX = (1 << 64) - 1


def unique(values):
    return list(dict.fromkeys(values))


def domain(family, options, ty, part):
    if ty in REGISTER_TYPES:
        values = list(range(32)) + [32, 63, 127, 128, 255]
    elif ty == "bool":
        values = [0, 1]
    elif ty == "Da64Cond":
        return list(range(16))
    elif ty == "enum Da64PrfOp":
        return list(range(32))
    else:
        values = [0, 1, 2, 7, 15, 31, 32, 63, 64, 127, 128, 255]
        if ty != "uint8_t":
            values += [65535, 65536, (1 << 31) - 1, 1 << 31, U32_MAX]
        if ty == "uint64_t":
            values += [1 << 32, (1 << 63) - 1, 1 << 63, U64_MAX]
    opts = {key: int(value, 0) for key, value in options.items()}
    for key in ["max", "base", "lim"]:
        if key in opts:
            limit = opts[key]
            values += [limit - 1, limit, limit + 1]
    if family in ["immvidx", "velemidx", "velemidxlim", "velemidx0", "memsimdidx"]:
        values += list(range((1 << (4 - opts["size"])) + 1))
    if family in ["immrotmul", "immrotadd"]:
        values += [angle + offset for angle in range(0, 361, 90) for offset in [-1, 0, 1]]
    if family == "tbz":
        values += [30, 33, 62, 65]
    if family == "uimm":
        quantum = 1 << opts["lsr"]
        high = ((1 << opts["bits"]) - 1) * quantum
        values += [quantum - 1, quantum, quantum + 1, high - quantum,
                   high - 1, high, high + 1, high + quantum]
    if family == "immsimd8lsl" and part == 1:
        values += list(range(0, 8 * opts["maxshift"] + 2))
    limit = 255 if ty in REGISTER_TYPES or ty == "uint8_t" else U64_MAX if ty == "uint64_t" else U32_MAX
    return unique(value for value in values if 0 <= value <= limit)


def parameters(form, functions):
    result, groups = [], []
    for operand in form["operands"].split():
        variables, spec = operand.split("=", 1)
        if spec.startswith("@"):
            continue
        family, _, option_text = spec.partition("/")
        options = dict(option.split("=", 1) for option in option_text.split(",") if option)
        start = len(result)
        for part, parameter in enumerate(functions[family].paramtys):
            ty, _, name = parameter.partition("  ")
            ty = ty.format(**options)
            assert ty in REGISTER_TYPES or ty in SCALAR_TYPES, (form["name"], family, ty)
            result.append({
                "name": name or variables.replace(",", ""), "type": ty,
                "family": family, "options": options, "part": part,
                "domain": domain(family, options, ty, part),
            })
        if len(result) != start:
            groups.append((family, options, list(range(start, len(result)))))
    return result, groups


def cases(params, groups, seed):
    baseline, register_indices = [], []
    for index, param in enumerate(params):
        family = param["family"]
        if param["type"] in REGISTER_TYPES:
            register_indices.append(index)
            value = len(register_indices) * 2
        elif family in ["immshiftr", "immshiftl"] or (family in ["immbfx", "immbfi"] and param["part"] == 1):
            value = 1
        elif family == "immrotadd":
            value = 90
        elif family == "immsimd8lsl" and param["part"] == 0:
            value = 0x5A
        else:
            value = 0
        baseline.append(value)
    result = {}

    def add(label, values):
        result.setdefault(tuple(values), label)

    add("baseline", baseline)
    for index, param in enumerate(params):
        for value in param["domain"]:
            values = baseline.copy()
            values[index] = value
            add(f"operand:{param['name']}={value:#x}", values)
    for family, options, indices in groups:
        if family in ["immbfx", "immbfi"]:
            maximum = int(options["max"], 0)
            edges = unique([0, 1, 2, maximum - 1, maximum, maximum + 1, U32_MAX])
            pairs = list(itertools.product(edges, repeat=2))
            pairs += [(lsb, maximum - lsb + extra)
                      for lsb in range(maximum) for extra in [0, 1]]
        elif family == "immsimd8lsl":
            pairs = itertools.product([0, 1, 31, 32, 63, 64, 127, 128, 255],
                                      [0, 1, 7, 8, 15, 16, 23, 24, 31, 32, U32_MAX])
        else:
            continue
        for pair in pairs:
            values = baseline.copy()
            for index, value in zip(indices, pair):
                values[index] = value
            add(f"coupled:{family}:{pair[0]}:{pair[1]}", values)
    for reg_index, reg in enumerate(params):
        if reg["family"] != "regfplim":
            continue
        for elem_index, elem in enumerate(params):
            if elem["family"] not in ["velemidx", "velemidxlim", "velemidx0"]:
                continue
            last = (1 << (4 - int(elem["options"]["size"], 0))) - 1
            for register, element in itertools.product([0, 7, 8, 15, 16, 31, 32], unique([0, 1, last])):
                values = baseline.copy()
                values[reg_index], values[elem_index] = register, element
                add(f"register-index:{register}:{element}", values)
    for left, right in itertools.combinations(register_indices, 2):
        values = baseline.copy()
        values[right] = values[left]
        add(f"register-equality:{left}:{right}", values)
        for base in register_indices:
            if params[base]["name"] == "Rn":
                bypass = values.copy()
                bypass[base] = 31
                add(f"register-equality-sp:{left}:{right}", bypass)
    for value in [0, 1, 30, 31]:
        values = baseline.copy()
        for index in register_indices:
            values[index] = value
        add(f"all-registers:{value}", values)
    state = seed
    for iteration in range(24):
        values = []
        for param in params:
            state ^= (state << 13) & U32_MAX
            state ^= state >> 17
            state ^= (state << 5) & U32_MAX
            values.append(param["domain"][state % len(param["domain"])])
        add(f"deterministic:{iteration}", values)
    return result.items()


def rust_argument(param, index):
    value = f"args[{index}]"
    ty = param["type"]
    if ty in REGISTER_TYPES:
        return f"runtime::{REGISTER_TYPES[ty]}({value} as u8)"
    if ty == "bool":
        return f"({value} != 0)"
    return f"({value} as {SCALAR_TYPES[ty]})"


def cpp_argument(param, index):
    ty = param["type"]
    if ty in REGISTER_TYPES:
        return f"{ty}{{static_cast<uint8_t>(args[{index}])}}"
    return f"static_cast<{ty}>(args[{index}])"


def emit(args):
    oracle.verify_sources()
    oracle.generate(argparse.Namespace(
        output=args.output, features="really-all", coverage=False,
        encode_in_header=args.encode_in_header, no_feature_desc=False,
        feature_desc=None, desc=None, model=True, classifications=False,
    ))
    model = json.loads((args.output / "model.json").read_text())
    functions = runpy.run_path(str(oracle.REFERENCE / "parse.py"), run_name="__encoder_cases__")["ENC_FUNCS"]
    forms = {form["name"]: form for form in model["encoder_forms"] if form["enabled"]}
    definitions = model["encoder_declarations"] if args.encode_in_header else model["encoder_definitions"]
    selected, excluded, emitted = [], [], set()
    for definition in definitions:
        if args.encode_in_header and not definition.startswith("static inline uint32_t "):
            continue
        match = DEFINITION.fullmatch(definition)
        assert match, definition
        name = match[2]
        assert name in forms and name not in emitted, name
        emitted.add(name)
        helpers = sorted(set(re.findall(r"\bda_\w+(?=\s*\()", match[4])))
        if helpers:
            excluded.append((name, helpers))
            continue
        params, groups = parameters(forms[name], functions)
        signature = ", ".join(f"{param['type']} {param['name']}" for param in params) or "void"
        assert signature == match[3], (name, signature, match[3])
        selected.append((name, definition, params, groups))
    assert selected and excluded and emitted <= forms.keys()
    maximum_args = max(len(params) for _, _, params, _ in selected)
    record_bytes = 4 + 8 * maximum_args
    rust_arms, cpp_arms, roster, corpus, labels = [], [], [], bytearray(), []
    offset = 0
    for encoder_id, (name, definition, params, groups) in enumerate(selected):
        rust_args = ", ".join(rust_argument(param, index) for index, param in enumerate(params))
        cpp_args = ", ".join(cpp_argument(param, index) for index, param in enumerate(params))
        rust_arms.append(f"{encoder_id} => generated::de64_{name}({rust_args}),")
        cpp_arms.append(f"case {encoder_id}: value = de64_{name}({cpp_args}); break;")
        first = offset
        for values, label in cases(params, groups, 0x13579BDF ^ (encoder_id + 1)):
            padded = list(values) + [0] * (maximum_args - len(values))
            corpus.extend(struct.pack(f"<I{maximum_args}Q", encoder_id, *padded))
            labels.append(f"{offset}\t{encoder_id}\t{name}\t{label}\t" + ",".join(map(str, values)) + "\n")
            offset += 1
        assert offset > first, name
        roster.append(f"{encoder_id}\t{name}\t{forms[name]['group']}\tSELECTED\t{first}\t{offset-first}\t\n")
    for name, helpers in excluded:
        roster.append(f"-\t{name}\t{forms[name]['group']}\tEXCLUDED_HELPER\t-\t0\t{','.join(helpers)}\n")
    for name in sorted(forms.keys() - emitted):
        roster.append(f"-\t{name}\t{forms[name]['group']}\tUPSTREAM_OMITTED\t-\t0\t\n")
    rust_source = f"""
fn main() {{
    use std::io::{{Read, Write}};
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input).unwrap();
    assert_eq!(input.len() % {record_bytes}, 0);
    let mut output = Vec::with_capacity(input.len() / {record_bytes} * 4);
    for record in input.chunks_exact({record_bytes}) {{
        let id = u32::from_le_bytes(record[..4].try_into().unwrap());
        let mut args = [0u64; {maximum_args}];
        for (arg, bytes) in args.iter_mut().zip(record[4..].chunks_exact(8)) {{
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
#include "disarm64.h"
{cpp_definitions}
int main() {{
    unsigned char record[{record_bytes}];
    size_t size;
    while ((size = std::fread(record, 1, sizeof(record), stdin)) != 0) {{
        if (size != sizeof(record)) return 2;
        uint32_t id = 0, value = 0;
        for (unsigned i = 0; i < 4; ++i) id |= uint32_t(record[i]) << (8 * i);
        uint64_t args[{maximum_args}] = {{}};
        for (unsigned i = 0; i < {maximum_args}; ++i)
            for (unsigned j = 0; j < 8; ++j)
                args[i] |= uint64_t(record[4 + i * 8 + j]) << (8 * j);
        switch (id) {{
            {chr(10).join(cpp_arms)}
            default: return 3;
        }}
        for (unsigned i = 0; i < 4; ++i) record[i] = value >> (8 * i);
        if (std::fwrite(record, 1, 4, stdout) != 4) return 4;
    }}
    return std::ferror(stdin) ? 5 : 0;
}}
"""
    (args.output / "disarm64.h").write_bytes((oracle.REFERENCE / "disarm64.h").read_bytes())
    (args.output / "disarm64-public.inc").write_bytes((args.output / "public.inc").read_bytes())
    (args.output / "encoder-main.rs").write_text(rust_source)
    (args.output / "encoder.cpp").write_text(cpp_source)
    (args.output / "encoder-input.bin").write_bytes(corpus)
    (args.output / "encoder-cases.tsv").write_text("case\tencoder\tname\tlabel\toperands\n" + "".join(labels))
    (args.output / "encoder-roster.tsv").write_text("encoder\tname\tgroup\tdisposition\tfirst_case\tcases\thelpers\n" + "".join(roster))
    (args.output / "encoder-counts.tsv").write_text(
        f"selected\t{len(selected)}\nexcluded\t{len(excluded)}\nomitted\t{len(forms)-len(emitted)}\n"
        f"cases\t{offset}\nrecord_bytes\t{record_bytes}\n"
    )


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("output", type=Path)
    parser.add_argument("--encode-in-header", action="store_true")
    emit(parser.parse_args())

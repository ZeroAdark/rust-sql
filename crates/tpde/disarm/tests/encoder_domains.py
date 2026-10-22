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


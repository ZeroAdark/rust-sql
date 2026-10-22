#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2024 Alexis Engelke
# SPDX-License-Identifier: BSD-3-Clause

import argparse
import hashlib
import json
from pathlib import Path
import re
import struct

import oracle


DECODE_BLOB = "80dcb446509213da11d26f3d133343d74dcc94e8"
OFFSET = 0xCBF29CE484222325
PRIME = 0x100000001B3
CANARY = 0x9E3779B97F4A7C15
STATE_MARKER = 0xD164DEC0DE012345
PURE = {"sext", "clz", "ctz"}
TYPES = {
    "unsigned": ("unsigned", "u32"),
    "bool": ("bool", "bool"),
    "uint8_t": ("uint8_t", "u8"),
    "uint16_t": ("uint16_t", "u16"),
    "uint32_t": ("uint32_t", "u32"),
    "uint64_t": ("uint64_t", "u64"),
    "int16_t": ("int16_t", "i16"),
    "int64_t": ("int64_t", "i64"),
    "enum Da64Ext": ("unsigned", "u32"),
    "struct Da64Inst*": ("struct Da64Inst*", "&mut Self::State"),
}


def helpers():
    data = (oracle.REFERENCE / "decode.c").read_bytes()
    blob = b"blob " + str(len(data)).encode() + b"\0" + data
    assert hashlib.sha1(blob).hexdigest() == DECODE_BLOB
    source = data.decode()
    pattern = re.compile(
        r"^static\s+(struct Da64Op|uint64_t|unsigned)\s+(\w+)\(([^)]*)\)\s*\{",
        re.MULTILINE,
    )
    result = []
    for match in pattern.finditer(source):
        returns, name, parameters = match.groups()
        if name == "immlogical":
            continue
        assert name in PURE or name.startswith("OP"), name
        arguments = []
        if parameters.strip():
            for parameter in parameters.split(","):
                parameter = " ".join(parameter.split())
                argument = re.fullmatch(r"(.+?)\s+(\w+)", parameter)
                assert argument, parameter
                kind, parameter_name = argument.groups()
                assert kind in TYPES, kind
                arguments.append((kind, parameter_name))
        end, depth = match.end(), 1
        while depth:
            depth += (source[end] == "{") - (source[end] == "}")
            end += 1
        result.append({
            "id": len(result) + 1,
            "name": name,
            "returns": returns,
            "arguments": arguments,
            "definition": source[match.start():end],
        })
    assert len(result) == 32
    assert {helper["name"] for helper in result if helper["name"] in PURE} == PURE
    return result


def write_cases(output, model):
    patterns = model["trie_descriptions"]
    assert patterns
    names = {number: name for name, number in model["mnemonics"]}
    count = 0
    state = 0x13579BDF
    with (output / "decoder-cases.bin").open("wb") as binary, (
        output / "decoder-cases.tsv"
    ).open("w") as roster:
        roster.write("case\tpattern\tmnemonic\tname\tpattern_name\tinst\tvalue\tmask\tinput\n")
        for index, (fixed, mnemonic, name, condition) in enumerate(patterns):
            value, mask = fixed
            assert condition is None
            assert mnemonic in names
            assert value & mask == value
            free = mask ^ 0xFFFFFFFF
            cases = [("base", value), ("all-free", value | free)]
            for bit in range(32):
                if free & (1 << bit):
                    cases.append((f"set-{bit}", value | (1 << bit)))
                    cases.append((f"clear-{bit}", value | (free ^ (1 << bit))))
            for iteration in range(8):
                state ^= (state << 13) & 0xFFFFFFFF
                state ^= state >> 17
                state ^= (state << 5) & 0xFFFFFFFF
                cases.append((f"random-{iteration}", value | (state & free)))
            for label, inst in cases:
                assert inst & mask == value
                binary.write(struct.pack("<II", inst, mnemonic))
                roster.write(
                    f"{count}\t{index}\t{mnemonic}\t{names[mnemonic]}\t{name}\t{inst:08x}\t"
                    f"{value:08x}\t{mask:08x}\t{label}\n"
                )
                count += 1
    return count


def c_reference(model, signatures):
    out = [f"""#include <stdint.h>
#include <stdio.h>
#include <initializer_list>
struct Da64Inst {{ uint64_t ops[5]; uint64_t identity; }};
static uint64_t trace = UINT64_C({OFFSET});
static uint64_t mix(uint64_t hash, uint64_t value) {{
  return (hash ^ value) * UINT64_C({PRIME});
}}
static uint64_t record(uint64_t id, std::initializer_list<uint64_t> arguments) {{
  uint64_t result = id;
  trace = mix(trace, id);
  for (uint64_t argument : arguments) {{
    trace = mix(trace, argument);
    result = mix(result, argument);
  }}
  return result;
}}
"""]
    out.extend(f"#define DA64_HAVE_{name} {int(enabled)}\n" for name, enabled in model["features"])
    for helper in signatures:
        if helper["name"] in PURE:
            out.append(helper["definition"] + "\n")
            continue
        parameters, arguments = [], []
        for kind, name in helper["arguments"]:
            parameters.append(f"{TYPES[kind][0]} {name}")
            arguments.append(
                f"{name}->identity" if kind == "struct Da64Inst*" else f"(uint64_t){name}"
            )
        out.append(
            f"static uint64_t {helper['name']}({', '.join(parameters)}) {{\n"
            f"  return record({helper['id']}, {{{', '.join(arguments)}}});\n}}\n"
        )
    for enum_name, prefix, key in [
        ("Da64InstKind", "DA64I_", "mnemonics"),
        ("Da64InstGroup", "DA64G_", "group_numbers"),
    ]:
        out.append(f"enum {enum_name} {{ {prefix}UNKNOWN = 0,\n")
        out.extend(f"  {prefix}{name} = {number},\n" for name, number in model[key])
        out.append("};\n")
    out.append(f"""#define DA64_GROUP(mnem) ((mnem) >> 7)
#define DA64_CLASSIFIER
#include "private.inc"
#undef DA64_CLASSIFIER
static void decode(uint32_t inst, unsigned mnem, struct Da64Inst* ddi) {{
  switch (DA64_GROUP(mnem)) {{
#define DA64_DECODER
#include "private.inc"
#undef DA64_DECODER
  }}
}}
static uint32_t load32(const unsigned char* data) {{
  return (uint32_t)data[0] | (uint32_t)data[1] << 8 |
         (uint32_t)data[2] << 16 | (uint32_t)data[3] << 24;
}}
int main() {{
  unsigned char input[8], output[48];
  size_t count;
  while ((count = fread(input, 1, sizeof(input), stdin)) != 0) {{
    if (count != sizeof(input)) return 2;
    uint32_t inst = load32(input), mnem = load32(input + 4);
    if (da64_classify_impl(inst) != mnem) return 3;
    Da64Inst state;
    state.identity = UINT64_C({STATE_MARKER});
    for (unsigned i = 0; i < 5; ++i) state.ops[i] = UINT64_C({CANARY}) ^ i;
    trace = UINT64_C({OFFSET});
    decode(inst, mnem, &state);
    uint64_t result[6] = {{trace, state.ops[0], state.ops[1], state.ops[2], state.ops[3], state.ops[4]}};
    for (unsigned i = 0; i < 6; ++i)
      for (unsigned byte = 0; byte < 8; ++byte)
        output[i * 8 + byte] = (unsigned char)(result[i] >> (byte * 8));
    if (fwrite(output, 1, sizeof(output), stdout) != sizeof(output)) return 4;
  }}
  return ferror(stdin) || fflush(stdout) != 0 ? 5 : 0;
}}
""")
    return "".join(out)


def rust_recorders(signatures):
    out = [f"""pub struct Recorder;
pub struct State {{ pub ops: [u64; 5], identity: u64 }}
std::thread_local! {{
    static TRACE: std::cell::Cell<u64> = const {{ std::cell::Cell::new({OFFSET}) }};
}}
fn mix(hash: u64, value: u64) -> u64 {{
    (hash ^ value).wrapping_mul({PRIME})
}}
fn record(id: u64, arguments: &[u64]) -> u64 {{
    TRACE.with(|trace| {{
        let mut hash = mix(trace.get(), id);
        let mut result = id;
        for &argument in arguments {{
            hash = mix(hash, argument);
            result = mix(result, argument);
        }}
        trace.set(hash);
        result
    }})
}}
pub fn reset() -> State {{
    TRACE.with(|trace| trace.set({OFFSET}));
    State {{ ops: std::array::from_fn(|i| {CANARY}u64 ^ i as u64), identity: {STATE_MARKER} }}
}}
pub fn finish(state: &State) -> [u64; 6] {{
    [TRACE.with(|trace| trace.get()), state.ops[0], state.ops[1], state.ops[2], state.ops[3], state.ops[4]]
}}
#[allow(non_snake_case)]
impl generated::DecoderHelpers for Recorder {{
    type State = State;
    type Op = u64;
    fn set_operand(ddi: &mut Self::State, index: usize, operand: Self::Op) {{
        ddi.ops[index] = operand;
    }}
"""]
    arithmetic = {
        "sext": "let sign = 1u64 << (bits - 1); if imm & sign != 0 { (imm ^ sign).wrapping_sub(sign) } else { imm }",
        "clz": "if v != 0 { v.leading_zeros().wrapping_add(sz).wrapping_sub(32) } else { sz }",
        "ctz": "v.trailing_zeros()",
    }
    for helper in signatures:
        parameters, arguments = [], []
        for kind, name in helper["arguments"]:
            parameters.append(f"{name}: {TYPES[kind][1]}")
            arguments.append(
                f"{name}.identity" if kind == "struct Da64Inst*" else f"{name} as u64"
            )
        returns = "Self::Op" if helper["returns"] == "struct Da64Op" else TYPES[helper["returns"]][1]
        body = arithmetic.get(helper["name"])
        if body is None:
            body = f"record({helper['id']}, &[{', '.join(arguments)}])"
        out.append(
            f"    fn {helper['name']}({', '.join(parameters)}) -> {returns} {{\n"
            f"        {body}\n    }}\n"
        )
    out.append("}\n")
    return "".join(out)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("output", type=Path)
    parser.add_argument("--features", choices=["all", "really-all"], default="really-all")
    parser.add_argument("--coverage", action="store_true")
    parser.add_argument("--desc", type=Path, action="append")
    args = parser.parse_args()
    oracle.verify_sources()
    signatures = helpers()
    oracle.generate(argparse.Namespace(
        output=args.output, features=args.features, no_feature_desc=False,
        feature_desc=None, coverage=args.coverage, encode_in_header=False,
        desc=args.desc, classifications=False, model=True,
    ))
    model = json.loads((args.output / "model.json").read_text())
    count = write_cases(args.output, model)
    (args.output / "decoder-reference.cpp").write_text(c_reference(model, signatures))
    (args.output / "decoder-recorders.rs").write_text(rust_recorders(signatures))
    roster = {
        "profile": args.features,
        "coverage": args.coverage,
        "cases": count,
        "patterns": len(model["trie_descriptions"]),
        "decode_git_blob": DECODE_BLOB,
        "helpers": [
            {key: value for key, value in helper.items() if key != "definition"}
            for helper in signatures
        ],
        "pure_helpers_recorded": False,
    }
    (args.output / "decoder-roster.json").write_text(json.dumps(roster, indent=2) + "\n")
    print(f"TPDE_DISARM_DECODER_CASES_READY cases={count} patterns={roster['patterns']}")


if __name__ == "__main__":
    main()

# SPDX-FileCopyrightText: 2018 Alexis Engelke
# SPDX-License-Identifier: BSD-3-Clause

import argparse
import hashlib
import itertools
import json
import os
import re
import shlex
import shutil
import subprocess
import traceback
from collections import Counter, defaultdict
from pathlib import Path


BASE = dict(flags=0, regs=[1, 2, 3, 4], mask=1, imm=1,
            absolute=0x1234567887654321, delta=66, memflags=0,
            base=3, scale=2, index=6, off=0, opcode=0x900401,
            ripoff=0, forcesib=0, disp8scale=0, size=0)
IMMEDIATES = [-2**63, -2**31-1, -2**31, -32769, -32768, -129,
              -128, -1, 0, 1, 2, 127, 128, 32767, 32768,
              2**24-1, 2**24, 2**31-1, 2**31, 2**63-1]
HELPERS = ["op_imm_n", "enc_seg67", "enc_rex_mem", "enc_imm", "enc_mem",
           "enc_vex_common", "enc_vex_reg", "enc_evex_reg", "enc_evex_xmm",
           "enc_vex_mem", "enc_evex_mem", "enc_vex_vsib", "enc_evex_vsib"]
FAMILIES = {
    "rex": r"unsigned rex",
    "high_byte": r"op_reg_gph",
    "lock": r"= 0xF0;",
    "prefix_66": r"= 0x66;",
    "prefix_f2": r"= 0xF2;",
    "prefix_f3": r"= 0xF3;",
    "opcode_register": r"buf\[idx-1\] \|=",
    "condition_code": r"flags(?:>>16|&FE_CC_MASK)",
    "rounding": r"flags&FE_RC_MASK",
    "relative_branch": r"\(int64_t\) buf",
    "relative_short": r"flags & FE_JMPL",
    "absolute_address": r"flags & FE_ADDR32 \? 4 : 8",
    "immediate": r"enc_imm\(",
    "memory": r"enc_(?:mem|(?:vex|evex)_(?:mem|vsib))\(",
    "vex": r"enc_vex_",
    "evex": r"enc_evex_",
    "masked": r"opmask",
    "vsib": r"enc_(?:vex|evex)_vsib\(",
}


def write(path, data):
    path.write_text(data, encoding="utf-8")


def checked(command, work, name, env=None):
    write(work / f"{name}.command.json", json.dumps(command) + "\n")
    with (work / f"{name}.stdout").open("wb") as stdout, (work / f"{name}.stderr").open("wb") as stderr:
        result = subprocess.run(command, stdout=stdout, stderr=stderr, env=env)
    if result.returncode:
        details = (work / f"{name}.stderr").read_text(errors="replace")
        raise AssertionError(f"{name}: exit {result.returncode}\n{details[-16000:]}")


def functions(public, private):
    declarations = {}
    for line in public.splitlines():
        if line.startswith("#define "):
            continue
        match = re.fullmatch(r"unsigned \((fe64_\w+)\)\((.*)\);", line)
        assert match, line
        name, arguments = match.groups()
        assert name not in declarations
        declarations[name] = arguments
    roster, position = [], 0
    while position < len(private):
        match = re.match(r"unsigned \((fe64_\w+)\)\(([^\n]*)\) \{", private[position:])
        assert match, private[position:position+200]
        name, arguments = match.groups()
        assert declarations.pop(name) == arguments
        start = position
        position += match.end()
        depth = 1
        while depth:
            depth += (private[position] == "{") - (private[position] == "}")
            position += 1
        body = private[start:position]
        arguments = arguments.split(", ")
        assert arguments[:2] == ["uint8_t* buf", "int flags"]
        operands = [tuple(argument.rsplit(" ", 1)) for argument in arguments[2:]]
        assert all(re.fullmatch(r"FeReg\w+|FeMemV?|int(?:8|16|32|64)_t|uintptr_t|const void\*", ty)
                   and re.fullmatch(r"opmask|op[0-3]", operand) for ty, operand in operands)
        roster.append((name, operands, body))
        while position < len(private) and private[position].isspace():
            position += 1
    assert not declarations and roster
    return roster


class Cases:
    def __init__(self):
        self.values, self.ids = [], {}
        self.add("baseline")

    def add(self, name, **changes):
        value = BASE | changes | {"name": name}
        if name in self.ids:
            assert self.values[self.ids[name]] == value, name
            return self.ids[name]
        number = len(self.values)
        self.values.append(value)
        self.ids[name] = number
        return number


def form_cases(roster, cases):
    selections, families, exclusions = [], [], []
    for number, (name, operands, body) in enumerate(roster):
        selected = [0]

        def add(label, **changes):
            selected.append(cases.add(label, **changes))

        if "FE_SEG_MASK" in body:
            for flags in [*range(1, 8), 16, *range(17, 24)]:
                add(f"prefix-{flags}", flags=flags)
        elif "FE_ADDR32" in body:
            add("prefix-16", flags=16)
        if "flags>>16" in body or "FE_CC_MASK" in body:
            for cc in range(16):
                add(f"condition-{cc}", flags=cc << 16)
        if "FE_RC_MASK" in body:
            for rc in range(4):
                add(f"rounding-{rc}", flags=rc << 5)
        if any(ty.startswith("FeReg") for ty, _ in operands):
            for value in [0, 7, 8, 15, 16, 31]:
                add(f"registers-{value}", regs=[value]*4)
        for ty, operand in operands:
            if operand == "opmask":
                for value in [0, 2, 7, 8]:
                    add(f"mask-{value}", mask=value)
            elif ty.startswith("FeReg"):
                index = int(operand[-1])
                values = range(8) if ty in ("FeRegSREG", "FeRegMASK") else [*range(9), 15, 16, 31]
                for value in values:
                    regs = list(BASE["regs"])
                    regs[index] = value
                    add(f"register-{index}-{value}", regs=regs)
                if ty == "FeRegGPLH":
                    for value in range(36, 40):
                        regs = [0]*4
                        regs[index] = value
                        add(f"high-{index}-{value}-low", regs=regs)
                        regs = [8]*4
                        regs[index] = value
                        add(f"high-{index}-{value}-rex", regs=regs, base=8)
            elif ty.startswith("int"):
                bits = int(re.search(r"\d+", ty)[0])
                for value in IMMEDIATES:
                    if -2**(bits-1) <= value < 2**(bits-1):
                        add(f"immediate-{value}", imm=value)
            elif ty == "uintptr_t":
                for value in [0, 0x7fffffff, 0x80000000, 0xffffffff, 0x100000000, 0x7fffffffffffffff]:
                    for flags in [0, 16]:
                        add(f"absolute-{value}-{flags}", absolute=value, flags=flags)
        if any(ty in ("FeMem", "FeMemV") for ty, _ in operands):
            for field, values in [("base", [0, 4, 5, 8, 13, 15, 16, 31, 32, 33, 128, 255]),
                                  ("index", [0, 4, 7, 8, 15, 16, 31, 128, 255]),
                                  ("scale", [0, 1, 3, 4, 5, 8, 9, 255]),
                                  ("off", [-2**31, -8193, -8192, -129, -128, -1, 1, 127, 128, 8191, 8192, 2**31-1]),
                                  ("memflags", [8, 64, 255])]:
                for value in values:
                    add(f"memory-{field}-{value}", **{field: value})
            for base in [0, 4, 5, 8, 13, 15, 32, 128]:
                for off in [-129, -128, 0, 127, 128]:
                    add(f"no-index-{base}-{off}", base=base, index=128, scale=0, off=off)
            for shift in range(1, 7):
                for bound in [-129, -128, -1, 0, 1, 127, 128]:
                    for residual in [0, 1]:
                        off = (bound << shift) + residual
                        add(f"compressed-{shift}-{bound}-{residual}", off=off)
        if "(int64_t) buf" in body:
            for delta in [-2**31-6, -2**31, -129, -128, -127, -126, -125, 0, 1, 2,
                          3, 127, 128, 129, 130, 131, 2**31-1, 2**31+5]:
                for flags in [0, 8, 16]:
                    add(f"relative-{delta}-{flags}", delta=delta, flags=flags)
        selected = list(dict.fromkeys(selected))
        assert not re.search(r"enc_(?:vex|evex)_(?:mem|vsib)\([^;\n]*, -1\)", body), \
            f"{name}: new undefined upstream displacement shift requires an exact reached-case analysis"
        selections.append(selected)
        for family, pattern in FAMILIES.items():
            if re.search(pattern, body):
                families.append((number, name, family, len(selected)))
    return selections, families, exclusions


def helper_cases(cases):
    result = []

    def add(helper, name, **changes):
        result.append((HELPERS.index(helper), cases.add("helper-" + name, **changes)))

    for size in range(10):
        for imm in IMMEDIATES:
            add("op_imm_n", f"imm-{size}-{imm}", size=size, imm=imm)
            if size <= 8:
                add("enc_imm", f"imm-{size}-{imm}", size=size, imm=imm)
    for flags in range(128):
        add("enc_seg67", f"prefix-{flags}", flags=flags)
    for base, index, reg, flags in itertools.product([0, 8, 15, 32, 128], [0, 8, 15, 31, 128], [0, 7, 8, 15, 31], [0, 8, 255]):
        add("enc_rex_mem", f"rex-{base}-{index}-{reg}-{flags}", base=base, index=index, regs=[reg, 0, 0, 0], memflags=flags)
    memory = []
    for field, values in [("base", [0, 4, 5, 8, 13, 15, 16, 31, 32, 33, 128, 255]),
                          ("index", [0, 4, 7, 8, 15, 16, 31, 128, 255]),
                          ("scale", [0, 1, 2, 3, 4, 5, 8, 9, 255])]:
        for value in values:
            memory.append((f"{field}-{value}", {field: value}))
    for base, index, scale in itertools.product([0, 5, 13, 32, 128], [4, 8, 16, 31, 128], [0, 1, 2, 8]):
        memory.append((f"address-{base}-{index}-{scale}", dict(base=base, index=index, scale=scale)))
    offsets = [-2**31, -8193, -8192, -129, -128, -1, 0, 1, 127, 128, 8191, 8192, 2**31-1]
    for shift, off, ripoff, forcesib in itertools.product([0, 1, 2, 4, 6], offsets, [0, 1, 7, 15], [0, 1]):
        add("enc_mem", f"mem-disp-{shift}-{off}-{ripoff}-{forcesib}", off=off, disp8scale=shift, ripoff=ripoff, forcesib=forcesib)
    for name, values in memory:
        for forcesib in [0, 1]:
            add("enc_mem", f"mem-{name}-{forcesib}", **values, forcesib=forcesib)
    opcodes = [0x900400, 0x900401, 0x900420, 0x900500, 0x900800, 0x908400,
               0x1900401, 0x3900401, 0x3908401, 0x900467]
    registers = [0, 7, 8, 15, 16, 31]
    for opcode, base, index, reg, vvvv in itertools.product(opcodes[:6], registers, registers, registers, registers):
        add("enc_vex_common", f"vex-{opcode}-{base}-{index}-{reg}-{vvvv}",
            opcode=opcode, base=base, index=index, regs=[reg, vvvv, 0, 0])
    for opcode, rm, reg, vvvv in itertools.product(opcodes, registers, registers, registers):
        for helper in ["enc_vex_reg", "enc_evex_reg", "enc_evex_xmm"]:
            add(helper, f"vector-reg-{opcode}-{rm}-{reg}-{vvvv}", opcode=opcode, base=rm, regs=[reg, vvvv, 0, 0])
    for helper, opcode, (name, values) in itertools.product(
            ["enc_vex_mem", "enc_evex_mem", "enc_vex_vsib", "enc_evex_vsib"], opcodes, memory):
        add(helper, f"vector-mem-{opcode}-{name}", opcode=opcode, **values)
    for helper, opcode, shift, off, reg in itertools.product(
            ["enc_vex_mem", "enc_evex_mem", "enc_vex_vsib", "enc_evex_vsib"],
            opcodes, [0, 1, 4, 6], offsets, [0, 8, 16, 31]):
        add(helper, f"vector-disp-{opcode}-{shift}-{off}-{reg}", opcode=opcode,
            disp8scale=shift, off=off, regs=[reg, 1, 0, 0], ripoff=3)
    return list(dict.fromkeys(result))


def integer(value):
    return "(-INT64_C(9223372036854775807)-1)" if value == -2**63 else f"INT64_C({value})"


def argument(ty, operand, body):
    if ty.startswith("FeReg"):
        value = "c.mask" if operand == "opmask" else f"c.regs[{operand[-1]}]"
        c = f"({ty}){{{value.replace('c.', 'c->')}}}"
        raw = f"<{ty} as RawRegister>::from_index({value})"
        public = f"{ty}::new({value}).ok()?"
        if ty == "FeRegGPLH":
            c = f"raw_gplh({value.replace('c.', 'c->')})"
            public = f"gplh({value})?"
        return c, raw, public
    if ty in ("FeMem", "FeMemV"):
        suffix = "v" if ty == "FeMemV" else ""
        return (f"({ty}){{c->memflags,{{c->base}},c->scale,{{c->index}},c->off}}",
                f"oracle::raw_mem{suffix}(c.memflags,c.base,c.scale,c.index,c.off)",
                f"memory{suffix}(c)?")
    if ty.startswith("int"):
        bits = re.search(r"\d+", ty)[0]
        return f"({ty})c->imm", f"c.imm as i{bits}", f"c.imm as i{bits}"
    if ty == "uintptr_t":
        return "c->absolute", "c.absolute as usize", "c.absolute as usize"
    assert ty == "const void*" and "(int64_t) buf" in body
    return ("(const void*)((uintptr_t)buf+(uintptr_t)c->delta)",
            "address.wrapping_add_signed(c.delta as isize)",
            "address.wrapping_add_signed(c.delta as isize)")


def sources(root, work, roster, selected, helper_selected, cases, build_directory):
    c_values, rust_values = [], []
    for case in cases.values:
        c_values.append("{" + ",".join([
            str(case["flags"]), "{" + ",".join(map(str, case["regs"])) + "}", str(case["mask"]),
            integer(case["imm"]), f"UINT64_C({case['absolute']})", integer(case["delta"])
        ] + [str(case[key]) for key in ("memflags", "base", "scale", "index", "off", "opcode", "ripoff", "forcesib", "disp8scale", "size")]) + "}")
        rust_values.append("Case{" + ",".join(
            f"{key}:" + ("[" + ",".join(map(str, value)) + "]" if key == "regs" else str(value))
            for key, value in case.items() if key != "name") + "}")
    c_calls, raw_calls, public_calls, class_rejections = [], [], [], {}
    for number, (name, operands, body) in enumerate(roster):
        args = [argument(ty, operand, body) for ty, operand in operands]
        tails = ["".join("," + arg[column] for arg in args) for column in range(3)]
        c_calls.append(f"case {number}: return ({name})(buf,(int)c->flags{tails[0]});")
        raw_calls.append(f"{number} => oracle::{name}::<Runtime>(buf,address,c.flags{tails[1]}),")
        public_calls.append(f"{number} => tpde_fadec::{name}(out,c.flags{tails[2]}),")
        if "enc_evex_" not in body:
            restricted = [f"c.regs[{operand[-1]}] >= 16" for ty, operand in operands if ty == "FeRegXMM"]
            if restricted:
                class_rejections[number] = f"{number} => " + " || ".join(restricted) + ","
    flat = list(itertools.chain.from_iterable(selected))
    offsets = [0]
    for values in selected:
        offsets.append(offsets[-1] + len(values))
    replacements = {
        "CASE_VALUES": ",\n".join(c_values),
        "FORM_CALLS": "\n".join(c_calls),
        "SELECTION_VALUES": ",".join(map(str, flat)),
        "OFFSET_VALUES": ",".join(map(str, offsets)),
        "HELPER_VALUES": ",".join("{" + str(helper) + "," + str(case) + "}" for helper, case in helper_selected),
        "FUNCTION_COUNT": str(len(roster)),
    }
    source = (root / "tests/harness/driver.c").read_text()
    group_size = 128
    groups, dispatch = [], []
    for group, start in enumerate(range(0, len(c_calls), group_size)):
        calls = "\n".join(c_calls[start:start + group_size])
        groups.append(
            f"__attribute__((noinline)) static unsigned invoke_group_{group}(unsigned function, uint8_t *buf, const Case *c) {{\n"
            f"switch (function) {{\n{calls}\ndefault: abort();\n}}\n}}")
        dispatch.append(f"case {group}: return invoke_group_{group}(function, buf, c);")
    for key, value in {
        "CXX_GROUP_FUNCTIONS": "\n\n".join(groups),
        "CXX_GROUP_DISPATCH": "\n".join(dispatch),
        "CXX_GROUP_SIZE": str(group_size),
    }.items():
        assert source.count(key) == 1, key
        source = source.replace(key, value)
    for key, value in replacements.items():
        assert key in source, key
        source = source.replace(key, value)
    write(work / "driver.c", source)
    rust_groups, raw_dispatch, public_dispatch, class_dispatch = [], [], [], []
    for group, start in enumerate(range(0, len(roster), group_size)):
        end = start + group_size
        raw = "\n".join(raw_calls[start:end])
        public = "\n".join(public_calls[start:end])
        rejected = "\n".join(class_rejections[number] for number in range(start, min(end, len(roster)))
                             if number in class_rejections)
        rust_groups.extend([
            f"#[inline(never)]\nfn invoke_group_{group}(function: usize, buf: &mut [u8; 32], c: &Case) -> usize {{\n"
            f"let address = ADDRESS;\nmatch function {{\n{raw}\n_ => unreachable!(),\n}}\n}}",
            f"#[inline(never)]\nfn invoke_public_group_{group}(function: usize, out: &mut Output<'_>, c: &Case) -> Option<Result<usize, EncodeError>> {{\n"
            f"let address = out.address();\nSome(match function {{\n{public}\n_ => unreachable!(),\n}})\n}}",
            f"#[inline(never)]\nfn class_rejection_group_{group}(function: usize, c: &Case) -> bool {{\n"
            f"match function {{\n{rejected}\n_ => false,\n}}\n}}",
        ])
        raw_dispatch.append(f"{group} => invoke_group_{group}(function, buf, c),")
        public_dispatch.append(f"{group} => invoke_public_group_{group}(function, out, c),")
        class_dispatch.append(f"{group} => class_rejection_group_{group}(function, c),")
    replacements |= {
        "CASE_VALUES": ",\n".join(rust_values),
        "FORM_CALLS": "\n".join(raw_dispatch),
        "PUBLIC_CALLS": "\n".join(public_dispatch),
        "HELPER_VALUES": ",".join(f"({helper},{case})" for helper, case in helper_selected),
        "ORACLE_BUILD_DIRECTORY_LITERAL": json.dumps(str(build_directory)),
        "PUBLIC_ADMISSION": "\n".join(class_dispatch),
        "RUST_GROUP_FUNCTIONS": "\n\n".join(rust_groups),
        "RUST_GROUP_SIZE": str(group_size),
    }
    source = (root / "tests/harness/driver.rs").read_text()
    for key, value in replacements.items():
        assert key in source, key
        source = source.replace(key, value)
    write(work / "driver.rs", source)


def compare(work, name, expected, roster, cases, candidate="rust"):
    counts, success, failed = Counter(), Counter(), Counter()
    with (work / f"c-{name}.stdout").open() as c_output, (work / f"{candidate}-{name}.stdout").open() as rust_output:
        for row, c_line, rust_line in itertools.zip_longest(expected, c_output, rust_output):
            assert row is not None and c_line is not None and rust_line is not None, f"{name} row count mismatch"
            assert c_line == rust_line, (name, row, roster[row[0]] if name == "forms" else "helper/NOP",
                                         cases.values[row[1]] if name != "nops" else "", c_line, rust_line)
            fields = c_line.rstrip("\n").split("\t")
            assert len(fields) == 4 and tuple(map(int, fields[:2])) == row
            result = int(fields[2])
            payload = fields[3]
            assert len(payload) % 2 == 0 and all(byte in "0123456789abcdef" for byte in payload)
            if name == "forms":
                assert 0 <= result <= 32 and len(payload) == 2*result
            elif name == "helpers":
                assert len(payload) == 64
            else:
                assert result == max(row[1], 1) and len(payload) == 2*result
            counts[row[0]] += 1
            (success if result else failed)[row[0]] += 1
    return counts, success, failed


def rust_command(root, work):
    profile = json.loads((Path(os.environ["TPDE_RUNNER_DIR"]) / "profiles/tpde-component.json").read_text())
    encoded = os.environ.get("CARGO_ENCODED_RUSTFLAGS")
    flags = encoded.split("\x1f") if encoded else shlex.split(os.environ.get("RUSTFLAGS", ""))
    sanitizer = any("sanitizer=address" in flag for flag in flags)
    toolchain = profile["miri" if sanitizer else "rust"]
    command = ["rustup", "run", toolchain, "rustc"]
    checked([*command, "-Vv"], work, "rustc-version")
    version = (work / "rustc-version.stdout").read_text()
    host = re.search(r"^host: (\S+)$", version, re.MULTILINE)
    assert host, version
    if sanitizer and "--target" not in flags and not any(flag.startswith("--target=") for flag in flags):
        flags += ["--target", host[1]]
    flags += ["-C", "lto=fat", "-C", "embed-bitcode=yes", "-C", "codegen-units=1"]
    write(work / "rust-codegen.json", json.dumps({"toolchain": toolchain, "flags": flags,
          "sanitizer_active": sanitizer, "host": host[1]}, indent=2) + "\n")
    return command, flags, sanitizer


def find_library(work, target_directory, build_directory, rustc, flags):
    assert target_directory.is_dir(), f"Cargo target directory is absent: {target_directory}"
    libraries = sorted((path for path in target_directory.rglob("libtpde_fadec*.rlib")
                        if re.fullmatch(r"libtpde_fadec(?:-[0-9a-f]+)?\.rlib", path.name)),
                       key=lambda path: path.stat().st_mtime_ns, reverse=True)
    assert libraries, f"no actual built tpde_fadec rlib in {target_directory}"
    probe = work / "probe.rs"
    write(probe, 'fn main() { assert_eq!(tpde_fadec::oracle::BUILD_DIRECTORY, ' + json.dumps(str(build_directory)) + '); '
          'let mut bytes = [0; 1]; let mut output = tpde_fadec::Output::new(&mut bytes, 0); '
          'assert_eq!(tpde_fadec::fe64_NOP(&mut output, 0), Ok(1)); assert_eq!(output.bytes(), &[0x90]); }\n')
    failures = []
    for number, library in enumerate(libraries):
        try:
            metadata = library.with_suffix(".rmeta")
            assert metadata.is_file(), f"matching Cargo metadata is absent: {metadata}"
            library_flags = ["--extern", f"tpde_fadec={library}", "--extern", f"tpde_fadec={metadata}",
                             "-L", f"dependency={library.parent}"]
            checked([*rustc, *flags, "--edition=2024", str(probe), *library_flags,
                     "-o", str(work / "probe")], work, f"probe-{number}-compile")
            checked([str(work / "probe")], work, f"probe-{number}-run")
        except AssertionError as error:
            failures.append(str(error))
        else:
            write(work / "linked-library.json", json.dumps({"path": str(library), "target_directory": str(target_directory),
                  "build_directory": str(build_directory),
                  "sha256": hashlib.sha256(library.read_bytes()).hexdigest(),
                  "metadata_path": str(metadata), "metadata_sha256": hashlib.sha256(metadata.read_bytes()).hexdigest(),
                  "extern_flags": library_flags}, indent=2) + "\n")
            return library_flags
    raise AssertionError("no matching exact Cargo build output:\n" + "\n".join(failures))


def run(args):
    root = Path(__file__).resolve().parent.parent
    work = args.work.resolve()
    work.mkdir(parents=True, exist_ok=True)
    write(work / "verdict.txt", "TPDE_FADEC_RUNTIME_ORACLE UNRUN\n")
    for stage in ["forms", "helpers", "nops"]:
        write(work / f"{stage}.verdict.txt", "UNRUN\n")
    for name in ["function-counts.tsv", "family-outcomes.tsv", "helper-counts.tsv", "scope.txt",
                 "linked-library.json", "rust-codegen.json", "roster.tsv", "cases.json", "families.tsv",
                 "c-undefined-cases.tsv", "excluded-cases.tsv", "helper-roster.tsv"]:
        (work / name).unlink(missing_ok=True)
    for pattern in ["*.stdout", "*.stderr", "*.command.json"]:
        for path in work.glob(pattern):
            path.unlink()
    reference = root / "tests/reference"
    manifest = json.loads((reference / "manifest.json").read_text())
    assert manifest["revision"] == "3994d89500985491b1a7ccc17827a22058b3de49"
    for row in manifest["files"]:
        data = (reference / row["path"]).read_bytes()
        assert hashlib.sha256(data).hexdigest() == row["sha256"], row["path"]
        assert hashlib.sha1(b"blob " + str(len(data)).encode() + b"\0" + data).hexdigest() == row["git_blob"], row["path"]
    shutil.copyfile(reference / "manifest.json", work / "reference-manifest.json")
    for filename in ["fadec-enc2.h", "encode2.c"]:
        shutil.copyfile(reference / filename, work / filename)
    checked(["python3", str(reference / "parseinstrs.py"), "--64", "--with-undoc", "encode2",
             str(reference / "instrs.txt"), str(work / "fadec-encode2-public.inc"),
             str(work / "fadec-encode2-private.inc")], work, "upstream-generate")
    roster = functions((work / "fadec-encode2-public.inc").read_text(), (work / "fadec-encode2-private.inc").read_text())
    rust_names = re.findall(r"pub fn (fe64_\w+)<", (args.build_directory / "raw.rs").read_text())
    assert len(rust_names) == len(set(rust_names)) == len(roster) >= 14000
    assert set(rust_names) == {name for name, _, _ in roster}
    cases = Cases()
    selected, families, exclusions = form_cases(roster, cases)
    helpers = helper_cases(cases)
    write(work / "cases.json", json.dumps(cases.values, indent=2) + "\n")
    write(work / "roster.tsv", "function_id\tfunction\tc_arguments\tcase_ids\n" + "".join(
        f"{number}\t{name}\t{','.join(ty+' '+operand for ty, operand in operands)}\t{','.join(map(str, selected[number]))}\n"
        for number, (name, operands, _) in enumerate(roster)))
    write(work / "families.tsv", "function_id\tfunction\tsource_family\tcases\n" + "".join(
        "\t".join(map(str, row)) + "\n" for row in families))
    write(work / "excluded-cases.tsv", "function_id\tfunction\tcase_id\treason\n" + "".join(
        "\t".join(map(str, row)) + "\n" for row in exclusions))
    write(work / "helper-roster.tsv", "helper_id\thelper\tcase_id\n" + "".join(
        f"{helper}\t{HELPERS[helper]}\t{case}\n" for helper, case in helpers))
    sources(root, work, roster, selected, helpers, cases, args.build_directory)
    rustc, rustflags, sanitizer = rust_command(root, work)
    library_flags = find_library(work, args.target_directory.resolve(), args.build_directory.resolve(), rustc, rustflags)
    profile = json.loads((Path(os.environ["TPDE_RUNNER_DIR"]) / "profiles/tpde-component.json").read_text())
    cxx = os.environ.get("CXX", profile["cxx"])
    checked([cxx, "--version"], work, "cxx-version")
    assert profile["cxx_version"] in (work / "cxx-version.stdout").read_text()
    compilers = [
        ("c-compile", [cxx, "-x", "c", "-std=c11", "-O3", "-I", str(work),
                       str(work / "driver.c"), "-o", str(work / "driver-c")]),
        ("cxx-sanitized-compile", [cxx, "-x", "c++", "-std=c++20", "-O1", "-g",
                                 "-fsanitize=address,undefined,bounds", "-fno-sanitize-recover=all", "-I", str(work),
                                 str(work / "driver.c"), "-o", str(work / "driver-cxx")]),
        ("rust-compile", [*rustc, *rustflags, "--edition=2024", "-C", "opt-level=0", "-C", "overflow-checks=yes",
                          str(work / "driver.rs"), *library_flags,
                          "-o", str(work / "driver-rust")]),
    ]
    compilation_failures = []
    for name, command in compilers:
        try:
            checked(command, work, name)
        except AssertionError as error:
            compilation_failures.append(str(error))
    assert not compilation_failures, "independent compiler failures:\n" + "\n".join(compilation_failures)
    if sanitizer:
        checked(["nm", str(work / "driver-rust")], work, "rust-sanitizer-symbols")
        assert "__asan_init" in (work / "rust-sanitizer-symbols.stdout").read_text()
    failures, totals = [], {}
    for name, expected in [("forms", [(number, case) for number, values in enumerate(selected) for case in values]),
                           ("helpers", helpers), ("nops", [(number, length) for number, length in enumerate(nop_lengths())])]:
        write(work / f"{name}.verdict.txt", "UNRUN\n")
        try:
            execution_failures = []
            for language in ["c", "cxx", "rust"]:
                try:
                    checked([str(work / f"driver-{language}"), name], work, f"{language}-{name}")
                except AssertionError as error:
                    execution_failures.append(str(error))
            assert not execution_failures, "\n".join(execution_failures)
            counts, success, failed = compare(work, name, iter(expected), roster, cases)
            compare(work, name, iter(expected), roster, cases, candidate="cxx")
            assert sum(counts.values()) == len(expected) > 0
            if name == "forms":
                assert len(counts) == len(roster) and len(expected) >= 600000
                write(work / "function-counts.tsv", "function_id\tfunction\texecutions\tencoded\trejected\n" + "".join(
                    f"{number}\t{function}\t{counts[number]}\t{success[number]}\t{failed[number]}\n"
                    for number, (function, _, _) in enumerate(roster)))
                assert all(success[number] > 0 for number in range(len(roster))), \
                    [roster[number][0] for number in range(len(roster)) if not success[number]]
                write(work / "family-outcomes.tsv", "source_family\tencoded\trejected\n" + "".join(
                    f"{family}\t{sum(success[number] for number, _, current, _ in families if current == family)}\t"
                    f"{sum(failed[number] for number, _, current, _ in families if current == family)}\n"
                    for family in FAMILIES))
            if name == "helpers":
                assert len(counts) == len(HELPERS)
                write(work / "helper-counts.tsv", "helper_id\thelper\texecutions\tnonzero\tzero\n" + "".join(
                    f"{number}\t{helper}\t{counts[number]}\t{success[number]}\t{failed[number]}\n"
                    for number, helper in enumerate(HELPERS)))
            totals[name] = len(expected)
            write(work / f"{name}.verdict.txt", f"PASS cases={len(expected)}\n")
            print(f"TPDE_FADEC_RUNTIME_{name.upper()} PASS cases={len(expected)}", flush=True)
        except Exception as error:
            failures.append(f"{name}: {error}")
            write(work / f"{name}.verdict.txt", "FAIL\n" + traceback.format_exc())
    write(work / "scope.txt", "Actual pinned encode2.c and all upstream-generated C functions compiled as C11 at O3 are the primary oracle. "
          "A second unchanged-source build as C++20 executes under ASan/UBSan/bounds and must match every C record. "
          "The language-mode distinction is deliberate: enc_rex_mem's promoted index << 24 can enter signed int's sign bit for "
          "the normal no-index sentinel, undefined in C but defined in C++20. This does not establish defined C semantics. "
          "The Rust driver links the current package rlib after checking its build directory, and invokes both actual raw generated functions "
          "with Runtime and checked production wrappers. Successful instruction lengths and live bytes compare; raw C failure scratch writes "
          "are unspecified, while the Rust wrapper must preserve its caller buffer on failure. Each typed-admitted case checks append PC and "
          "canaries; every function with an admitted successful case checks exact and insufficient capacity plus reuse. Dedicated helper matrices "
          "compare complete scratch contents and return values. Relative destinations are based on each C buffer and the equivalent logical Rust "
          "PC. Header constructors intentionally reject malformed classes before raw encoding; those cases remain in the raw differential. "
          "The generated roster is required to contain no negative displacement-shift call; no form cases are excluded. "
          "These are byte-encoding checks, not native x86 instruction execution or a performance result.\n")
    if failures:
        write(work / "verdict.txt", "TPDE_FADEC_RUNTIME_ORACLE FAIL\n" + "\n".join(failures) + "\n")
        raise AssertionError("independent runtime stages failed:\n" + "\n".join(failures))
    verdict = (f"TPDE_FADEC_RUNTIME_ORACLE PASS functions={len(roster)} forms={totals['forms']} "
               f"helper_cases={totals['helpers']} helpers={len(HELPERS)} nops={totals['nops']} "
               f"excluded_cases={len(exclusions)} rust_asan={int(sanitizer)} runtime=actual-encode2\n")
    write(work / "verdict.txt", verdict)
    print(verdict, end="", flush=True)


def nop_lengths():
    return [*range(129), 255, 256, 511, 512, 1023, 1024, 4095, 4096, 65535, 65536]


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--work", required=True, type=Path)
    parser.add_argument("--target-directory", required=True, type=Path)
    parser.add_argument("--build-directory", required=True, type=Path)
    arguments = parser.parse_args()
    try:
        run(arguments)
    except Exception:
        arguments.work.mkdir(parents=True, exist_ok=True)
        write(arguments.work / "verdict.txt", "TPDE_FADEC_RUNTIME_ORACLE FAIL\n" + traceback.format_exc())
        raise

# SPDX-FileCopyrightText: 2018 Alexis Engelke
# SPDX-License-Identifier: BSD-3-Clause

import argparse
import csv
import hashlib
import itertools
import json
import re
import shutil
import subprocess
import traceback
from collections import Counter, defaultdict
from pathlib import Path


HELPERS = dict(enumerate(("op_imm_n", "enc_seg67", "enc_rex_mem", "enc_imm",
    "enc_mem", "enc_vex_common", "enc_vex_reg", "enc_evex_reg", "enc_evex_xmm",
    "enc_vex_mem", "enc_evex_mem", "enc_vex_vsib", "enc_evex_vsib"), 1))
BASELINE = dict(flags=0, regs=[1, 2, 3, 4], mask=1, imm=1,
    absolute=0x1234567887654321, delta=66, memflags=0, base=3, scale=2,
    index=6, off=0x12345678, failure=0, variant=0)
FAMILIES = {
    "legacy_rex": r"unsigned rex",
    "high_byte_rejection": r"op_reg_gph",
    "legacy_fixed_modrm": r"buf\[idx\+\+\] = 0x[c-f][0-9a-f];",
    "legacy_register_modrm": r"0xC0\|",
    "opcode_register": r"buf\[idx-1\] \|=",
    "prefix_lock": r"= 0xF0;",
    "prefix_66": r"= 0x66;",
    "prefix_f2": r"= 0xF2;",
    "prefix_f3": r"= 0xF3;",
    "escape_38": r"= 0x38;",
    "escape_3a": r"= 0x3A;",
    "segment_address_prefix": r"enc_seg67\(",
    "address_prefix_store": r"\*buf=0x67",
    "segment_variant": r"op_reg_idx\(op0\)==0x",
    "xchg_zero_pair": r"!\(op_reg_idx\(op0\)==0&&",
    "zero_register_variant": r"op_reg_idx\(op\d\)==[0-9]+",
    "immediate_one": r"op\d == 1",
    "absolute_address_width": r"flags & FE_ADDR32 \? 4 : 8",
    "immediate_register_rejection": r"op_reg_idx\(op\d\) >= 16",
    "enter_24bit": r">= 0x1000000",
    "immediate_narrowing": r"op_imm_n\(\(int64_t\) op\d,",
    "relative_branch": r"\(int64_t\) buf",
    "relative_short_preference": r"flags & FE_JMPL",
    "masked_zero_rejection": r"!op_reg_idx\(opmask\)",
    "condition_code": r"flags(?:>>16|&FE_CC_MASK)",
    "rounding_control": r"flags&FE_RC_MASK",
    "legacy_memory_adjustment": r"idx -=",
    "vector_memory_immediate": r"if \(instlen\) enc_imm",
    "vector_register_immediate": r"return vexoff \? vexoff\+",
    "vector_register_prefix_adjustment": r"return vexoff \? vexoff\+idx",
}


def write(path, data):
    path.write_text(data, encoding="utf-8")


def checked(command, target, label):
    write(target / (label + ".command.json"), json.dumps(command) + "\n")
    with (target / (label + ".stdout")).open("wb") as stdout, (target / (label + ".stderr")).open("wb") as stderr:
        result = subprocess.run(command, stdout=stdout, stderr=stderr)
    if result.returncode:
        message = (target / (label + ".stderr")).read_text(errors="replace")
        raise AssertionError(f"{label} failed ({result.returncode}): {message[-12000:]}")


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
    result = []
    position = 0
    while position < len(private):
        match = re.match(r"unsigned \((fe64_\w+)\)\(([^\n]*)\) \{", private[position:])
        assert match, private[position:position + 200]
        name, arguments = match.groups()
        assert declarations.pop(name) == arguments
        start = position
        position += match.end()
        depth = 1
        while depth:
            depth += (private[position] == "{") - (private[position] == "}")
            position += 1
        body = private[start:position]
        args = arguments.split(", ")
        assert args[:2] == ["uint8_t* buf", "int flags"]
        operands = [tuple(argument.rsplit(" ", 1)) for argument in args[2:]]
        assert all(re.fullmatch(r"FeReg\w+|FeMemV?|int(?:8|16|32|64)_t|uintptr_t|const void\*", ty)
                   and re.fullmatch(r"opmask|op[0-3]", operand) for ty, operand in operands)
        result.append((name, operands, body))
        while position < len(private) and private[position].isspace():
            position += 1
    assert not declarations and result
    return result


class Cases:
    def __init__(self):
        self.values = []
        self.ids = {}
        self.add("baseline")

    def add(self, name, **changes):
        value = BASELINE | changes
        value["name"] = name
        if name in self.ids:
            assert self.values[self.ids[name]] == value, name
            return self.ids[name]
        number = len(self.values)
        self.ids[name] = number
        self.values.append(value)
        return number


def select_cases(roster, cases):
    selections, family_rows = [], []
    for function, (name, operands, body) in enumerate(roster):
        chosen = [0]

        def add(label, **changes):
            chosen.append(cases.add(label, **changes))

        if "FE_SEG_MASK" in body:
            for flags in [*range(1, 7), 16, *range(17, 23)]:
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
            add("all-registers-zero", regs=[0] * 4)
            add("all-registers-eight", regs=[8] * 4)
        for ty, operand in operands:
            if operand == "opmask":
                for mask in [0, 2, 7]:
                    add(f"mask-{mask}", mask=mask)
            elif ty.startswith("FeReg"):
                index = int(operand[-1])
                values = range(8) if ty in ("FeRegSREG", "FeRegMASK") else [0, 4, 7, 8, 15, 16, 31]
                for value in values:
                    registers = list(BASELINE["regs"])
                    registers[index] = value
                    add(f"reg-{index}-{value}", regs=registers)
                if ty == "FeRegGPLH":
                    for value in range(36, 40):
                        registers = [0] * 4
                        registers[index] = value
                        add(f"high-{index}-{value}-low", regs=registers)
                        registers = [8] * 4
                        registers[index] = value
                        add(f"high-{index}-{value}-rex", regs=registers, base=8)
            elif ty.startswith("int"):
                bits = int(re.search(r"\d+", ty)[0])
                for value in [-2**63, -2**31-1, -2**31, -32769, -32768, -129, -128,
                              -1, 0, 1, 2, 127, 128, 32767, 32768, 2**24-1, 2**24,
                              2**31-1, 2**31, 2**63-1]:
                    if -(2**(bits-1)) <= value < 2**(bits-1):
                        add(f"immediate-{value}", imm=value)
            elif ty == "uintptr_t":
                for value in [0, 0x7fffffff, 0x80000000, 0xffffffff, 0x100000000, 0x7fffffffffffffff]:
                    for flags in [0, 16]:
                        add(f"absolute-{value}-{flags}", absolute=value, flags=flags)
        if any(ty in ("FeMem", "FeMemV") for ty, _ in operands):
            for field, values in [("base", [8, 13, 32, 128]), ("index", [4, 8, 31, 128]),
                                  ("scale", [0, 1, 4, 8]), ("off", [-129, -128, 0, 127, 128]),
                                  ("memflags", [8])]:
                for value in values:
                    add(f"memory-{field}-{value}", **{field: value})
        if "(int64_t) buf" in body:
            for delta in [-2**31-6, -2**31, -129, -128, -127, -126, -125, 0, 1, 2,
                          3, 127, 128, 129, 130, 131, 2**31-1, 2**31+5]:
                for flags in [0, 8, 16]:
                    add(f"relative-{delta}-{flags}", delta=delta, flags=flags)
        helper_ids = [number for number, helper in HELPERS.items() if re.search(r"\b" + helper + r"\(", body)]
        if any(number >= 5 for number in helper_ids):
            add("helper-length-five", variant=1)
            add("helper-length-five-prefix", variant=1, flags=21)
        for helper in helper_ids:
            if helper >= 5:
                add(f"helper-failure-{helper}", failure=helper)
                add(f"helper-failure-{helper}-prefix", failure=helper, flags=21)
        selected = list(dict.fromkeys(chosen))
        selections.append(selected)
        families = [family for family, pattern in FAMILIES.items() if re.search(pattern, body)]
        families += ["helper_" + HELPERS[number] for number in helper_ids]
        if not helper_ids:
            families.append("helper_free_bytes")
        for family in families:
            family_rows.append((function, name, family, len(selected)))
    return selections, family_rows


def c_integer(value):
    return "(-INT64_C(9223372036854775807)-1)" if value == -2**63 else f"INT64_C({value})"


def sources(target, roster, selections, cases):
    c_cases, rust_cases, c_calls, rust_calls = [], [], [], []
    for case in cases.values:
        c_cases.append("{" + ",".join([str(case["flags"]), "{" + ",".join(map(str, case["regs"])) + "}",
            str(case["mask"]), c_integer(case["imm"]), f'UINT64_C({case["absolute"]})',
            c_integer(case["delta"])] + [str(case[key]) for key in
                ("memflags", "base", "scale", "index", "off", "failure", "variant")]) + "}")
        rust_cases.append("Case{" + ",".join(f"{key}:" + ("[" + ",".join(map(str, value)) + "]"
            if key == "regs" else str(value)) for key, value in case.items() if key != "name") + "}")
    for number, (name, operands, body) in enumerate(roster):
        c_args, rust_args = [], []
        for ty, operand in operands:
            if ty.startswith("FeReg"):
                value = "c->mask" if operand == "opmask" else f"c->regs[{operand[-1]}]"
                c_args.append(f"({ty}){{{value}}}")
                rust_args.append(f"{ty}{{idx:{value.replace('c->', 'c.')}}}")
            elif ty in ("FeMem", "FeMemV"):
                c_args.append(f"({ty}){{c->memflags,{{c->base}},c->scale,{{c->index}},c->off}}")
                index_ty = "FeRegXMM" if ty == "FeMemV" else "FeRegGP"
                rust_args.append(f"{ty}{{flags:c.memflags,base:FeRegGP{{idx:c.base}},scale:c.scale,idx:{index_ty}{{idx:c.index}},off:c.off}}")
            elif ty.startswith("int"):
                bits = re.search(r"\d+", ty)[0]
                c_args.append(f"({ty})c->imm")
                rust_args.append(f"c.imm as i{bits}")
            elif ty == "uintptr_t":
                c_args.append("c->absolute")
                rust_args.append("c.absolute as usize")
            else:
                assert ty == "const void*" and "(int64_t) buf" in body
                c_args.append("(const void*)((intptr_t)buf+c->delta)")
                rust_args.append("address.wrapping_add_signed(c.delta as isize)")
        c_tail = "".join("," + value for value in c_args)
        rust_tail = "".join("," + value for value in rust_args)
        c_calls.append(f"case {number}: return ({name})(buf,(int)c->flags{c_tail});")
        rust_calls.append(f"{number} => {name}::<Recorder>(buf,address,c.flags{rust_tail}),")
    offsets = [0]
    flat = []
    for selected in selections:
        flat += selected
        offsets.append(len(flat))
    c_driver = r'''
typedef struct Case {
    unsigned flags; uint8_t regs[4], mask; int64_t imm; uintptr_t absolute;
    int64_t delta; uint8_t memflags, base, scale, index; int32_t off;
    unsigned failure, variant;
} Case;
static const Case cases[] = {CASES};
static const unsigned selected[] = {SELECTED};
static const unsigned offsets[] = {OFFSETS};
static unsigned invoke(unsigned function, uint8_t *buf, const Case *c) {
    switch (function) {CALLS default: assert(false); return 0; }
}
int main(void) {
    setvbuf(stdout, NULL, _IOFBF, 1024 * 1024);
    for (unsigned function = 0; function + 1 < sizeof(offsets)/sizeof(offsets[0]); ++function) {
        for (unsigned run = offsets[function]; run < offsets[function+1]; ++run) {
            unsigned caseid = selected[run];
            const Case *c = &cases[caseid];
            uint8_t storage[64];
            memset(storage, 0x5a, sizeof storage);
            uint8_t *buf = storage + 16;
            memset(buf, 0xa5, 32);
            assert((uintptr_t)buf >= UINT64_C(0x100000000));
            assert((uintptr_t)buf < UINT64_C(0x7fffffff00000000));
            recording_reset(buf, c->failure, c->variant);
            unsigned result = invoke(function, buf, c);
            for (unsigned i = 0; i < 16; ++i) assert(storage[i] == 0x5a && storage[48+i] == 0x5a);
            recording_finish(function, caseid, result);
        }
    }
    assert(fflush(stdout) == 0);
}
'''.replace("CASES", ",\n".join(c_cases)).replace("SELECTED", ",".join(map(str, flat))) \
       .replace("OFFSETS", ",".join(map(str, offsets))).replace("CALLS", "\n".join(c_calls))
    rust_driver = r'''
struct Case {
    flags: u32, regs: [u8;4], mask: u8, imm: i64, absolute: u64, delta: i64,
    memflags: u8, base: u8, scale: u8, index: u8, off: i32, failure: u32, variant: u32,
}
static CASES: &[Case] = &[CASE_VALUES];
static SELECTED: &[usize] = &[SELECTION_VALUES];
static OFFSETS: &[usize] = &[OFFSET_VALUES];
fn invoke(function: usize, buf: &mut [u8;32], c: &Case) -> usize {
    let address = 0x1_0000_0000usize;
    match function {CALLS _ => unreachable!() }
}
fn main() {
    let stdout = std::io::stdout();
    let mut output = std::io::BufWriter::with_capacity(1024*1024, stdout.lock());
    for function in 0..OFFSETS.len()-1 {
        for &caseid in &SELECTED[OFFSETS[function]..OFFSETS[function+1]] {
            let case = &CASES[caseid];
            let mut buf = [0xa5;32];
            recording_reset(&buf, case.failure, case.variant);
            let result = invoke(function, &mut buf, case);
            recording_finish(&mut output, function, caseid, result, &buf);
        }
    }
    std::io::Write::flush(&mut output).unwrap();
}
'''.replace("CASE_VALUES", ",\n".join(rust_cases)).replace("SELECTION_VALUES", ",".join(map(str, flat))) \
       .replace("OFFSET_VALUES", ",".join(map(str, offsets))).replace("CALLS", "\n".join(rust_calls))
    write(target / "driver.c", c_driver)
    write(target / "driver.rs", rust_driver)


def compare(target, roster, selections, cases, family_rows):
    counts, helpers, families = Counter(), defaultdict(Counter), defaultdict(Counter)
    expected = ((function, case) for function, selected in enumerate(selections) for case in selected)
    body_families = defaultdict(list)
    for function, _, family, _ in family_rows:
        body_families[function].append(family)
    with (target / "c-run.stdout").open() as c_output, (target / "rust-run.stdout").open() as rust_output:
        for expected_row, c_line, rust_line in itertools.zip_longest(expected, c_output, rust_output):
            assert expected_row is not None and c_line is not None and rust_line is not None, "execution row count mismatch"
            function, caseid = expected_row
            assert c_line == rust_line, (target.name, roster[function][0], cases.values[caseid], c_line, rust_line)
            fields = c_line.rstrip("\n").split("\t")
            assert len(fields) == 5 and tuple(map(int, fields[:2])) == expected_row
            result = int(fields[2])
            assert 0 <= result <= 32 and len(fields[3]) == 64
            counts[function] += 1
            if caseid == 0:
                counts["baseline"] += 1
            outcome = "success" if result else "zero"
            for family in body_families[function]:
                families[family][outcome] += 1
            for event in fields[4].split(";"):
                if not event:
                    continue
                values = list(map(int, event.split(",")))
                helper, offset, *arguments, returned = values
                assert helper in HELPERS and -1 <= offset <= 32
                helper_outcome = "zero" if returned == 0 else "nonzero"
                helpers[helper][helper_outcome] += 1
                if cases.values[caseid]["failure"] == helper:
                    assert returned == result == 0
                    helpers[helper]["injected_failure"] += 1
    assert counts["baseline"] == len(roster)
    for function, selected in enumerate(selections):
        assert counts[function] == len(selected), roster[function][0]
    called = {number for number, name in HELPERS.items()
              if any(re.search(r"\b" + name + r"\(", body) for _, _, body in roster)}
    assert set(helpers) == called
    for helper in called:
        if helper >= 5:
            assert helpers[helper]["injected_failure"] and helpers[helper]["nonzero"], HELPERS[helper]
    write(target / "helper-outcomes.tsv", "helper\tzero\tnonzero\tinjected_failure\n" + "".join(
        f"{HELPERS[helper]}\t{value['zero']}\t{value['nonzero']}\t{value['injected_failure']}\n"
        for helper, value in sorted(helpers.items())))
    write(target / "family-outcomes.tsv", "source_family\tcase_success\tcase_zero\n" + "".join(
        f"{family}\t{value['success']}\t{value['zero']}\n" for family, value in sorted(families.items())))
    write(target / "function-counts.tsv", "function_id\tfunction\tbaseline\texecutions\n" + "".join(
        f"{number}\t{name}\t1\t{counts[number]}\n" for number, (name, _, _) in enumerate(roster)))
    return sum(len(selected) for selected in selections), set(families)


def profile(root, source, target):
    target.mkdir(exist_ok=True)
    for filename in ["reference.public", "reference.private", "generated.rs"]:
        shutil.copyfile(source / filename, target / filename)
    for filename in ["runtime.c", "runtime.rs"]:
        shutil.copyfile(root / "tests/generated-execution" / filename, target / filename)
    roster = functions((target / "reference.public").read_text(), (target / "reference.private").read_text())
    rust_names = re.findall(r"pub fn (fe64_\w+)<", (target / "generated.rs").read_text())
    assert len(rust_names) == len(set(rust_names)) == len(roster)
    assert set(rust_names) == {name for name, _, _ in roster}
    cases = Cases()
    selections, family_rows = select_cases(roster, cases)
    write(target / "cases.json", json.dumps(cases.values, indent=2) + "\n")
    write(target / "roster.tsv", "function_id\tfunction\tc_arguments\tcase_ids\n" + "".join(
        f"{number}\t{name}\t{','.join(ty+' '+operand for ty, operand in operands)}\t{','.join(map(str, selections[number]))}\n"
        for number, (name, operands, _) in enumerate(roster)))
    write(target / "source-families.tsv", "function_id\tfunction\tsource_family\tselected_cases\n" + "".join(
        "\t".join(map(str, row)) + "\n" for row in family_rows))
    sources(target, roster, selections, cases)
    write(target / "execution.c", (target / "runtime.c").read_text() + "\n" +
          (target / "reference.private").read_text() + "\n" + (target / "driver.c").read_text())
    write(target / "execution.rs", (target / "generated.rs").read_text() + "\n" +
          (target / "runtime.rs").read_text() + "\n" + (target / "driver.rs").read_text())
    checked(["cc", "-std=c11", "-O0", "-fsanitize=undefined,bounds", "-fno-sanitize-recover=all",
             str(target / "execution.c"), "-o", str(target / "execution-c")], target, "c-compile")
    checked(["rustc", "--edition=2024", "-C", "opt-level=0", "-C", "overflow-checks=yes",
             str(target / "execution.rs"), "-o", str(target / "execution-rust")], target, "rust-compile")
    checked([str(target / "execution-c")], target, "c-run")
    checked([str(target / "execution-rust")], target, "rust-run")
    count, families = compare(target, roster, selections, cases, family_rows)
    return len(roster), count, families


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--work", required=True, type=Path)
    work = parser.parse_args().work.resolve()
    root = Path(__file__).resolve().parent.parent
    output = work / "generated-execution"
    output.mkdir(exist_ok=True)
    write(output / "verdict.txt", "FADEC_GEN_EXECUTION_UNRUN\n")
    write(output / "case-count.txt", "0\n")
    profiles = list(csv.DictReader((work / "profiles.tsv").open(), delimiter="\t"))
    groups = {}
    mapping = ["profile\tcontent_sha256\texecution_artifact\tverdict\tbaseline_functions\texecution_cases\n"]
    failures = []
    input_failures = 0
    requested = 0
    for row in profiles:
        if row["mode"] != "encode2":
            continue
        requested += 1
        source = work / row["profile"]
        digest = hashlib.sha256()
        try:
            for name in ["reference.public", "reference.private", "generated.rs"]:
                data = (source / name).read_bytes()
                digest.update(len(data).to_bytes(8, "little"))
                digest.update(data)
        except Exception as error:
            failures.append(f"{row['profile']} input: {error}")
            input_failures += 1
            mapping.append(f"{row['profile']}\t\t\tINPUT_FAIL\t\t\n")
            continue
        groups.setdefault(digest.hexdigest(), []).append(row)
    if requested != 8:
        failures.append(f"encode2 profile roster: expected 8, found {requested}")
    assert sum(map(len, groups.values())) + input_failures == requested
    total, total_functions, families = 0, 0, set()
    failed_groups = 0
    for number, (digest, aliases) in enumerate(groups.items()):
        target = output / f"unique-{number}"
        target.mkdir(exist_ok=True)
        write(target / "verdict.txt", f"FADEC_GEN_EXECUTION_GROUP_UNRUN group={target.name}\n")
        (target / "failure.txt").unlink(missing_ok=True)
        try:
            function_count, case_count, found = profile(root, work / aliases[0]["profile"], target)
        except Exception as error:
            failures.append(f"{target.name}: {error}")
            failed_groups += 1
            write(target / "verdict.txt", f"FADEC_GEN_EXECUTION_GROUP_FAIL group={target.name}\n{error}\n")
            write(target / "failure.txt", traceback.format_exc())
            verdict, function_count, case_count = "FAIL", "", ""
        else:
            total += case_count
            total_functions += function_count
            families.update(found)
            verdict = "PASS"
            write(target / "verdict.txt", f"FADEC_GEN_EXECUTION_GROUP_PASS group={target.name} "
                  f"baseline_functions={function_count} cases={case_count}\n")
        for alias in aliases:
            mapping.append(f"{alias['profile']}\t{digest}\t{target.name}\t{verdict}\t{function_count}\t{case_count}\n")
    write(output / "source-family-admission.tsv", "source_family\tadmission\n" + "".join(
        f"{family}\t{'EXECUTED_SOURCE_FAMILY' if family in families else 'NOT_ESTABLISHED' if failures else 'SOURCE_PATTERN_ABSENT'}\n"
        for family in FAMILIES))
    write(output / "profile-map.tsv", "".join(mapping))
    write(output / "case-count.txt", str(total) + "\n")
    write(output / "scope.txt", "Generated C bodies execute unchanged with matching test recording helpers. "
          "Exact helper arguments, offsets, return values, full scratch, and generated-function lengths compare. "
          "C canaries and UBSan/bounds instrumentation are enabled; Rust checks overflow and slice bounds. "
          "Relative targets use each C buffer's actual pointer and the equivalent Rust logical address. "
          "Signed C subtraction overflow is excluded by address bounds and bounded deltas. "
          "The Rust i32 displacement-scale argument is recorded as its u32 bit pattern to match the C unsigned parameter. "
          "Family outcomes label selected source families, not measured per-branch coverage. "
          "Marker bytes are helper fingerprints, not real VEX/EVEX/memory encoder output. "
          "Actual runtime encoded-byte equivalence, runtime admission, and performance remain OPEN.\n")
    if failures:
        verdict = (f"FADEC_GEN_EXECUTION_FAIL unique_profiles={len(groups)} failed_groups={failed_groups} "
                   f"input_failures={input_failures} passed_groups={len(groups)-failed_groups} checked_baseline_functions={total_functions} "
                   f"checked_cases={total}\n")
        write(output / "verdict.txt", verdict)
        print(verdict, end="")
        raise AssertionError("independent execution groups failed:\n" + "\n".join(failures))
    verdict = (f"FADEC_GEN_EXECUTION_PASS unique_profiles={len(groups)} "
               f"mapped_profiles={sum(map(len, groups.values()))} baseline_functions={total_functions} "
               f"cases={total} source_families={len(families)} runtime=recording-helper-contract\n")
    write(output / "verdict.txt", verdict)
    print(verdict, end="")


if __name__ == "__main__":
    main()

# SPDX-FileCopyrightText: 2018 Alexis Engelke
# SPDX-License-Identifier: BSD-3-Clause

import argparse
import ast
import contextlib
import csv
import hashlib
import importlib.util
import io
import json
import re
import subprocess
import sys
from collections import Counter
from pathlib import Path
from types import SimpleNamespace


REVISION = "3994d89500985491b1a7ccc17827a22058b3de49"
NEGATIVES = {
    "bad_opcode": "zz NP - - - - BAD",
    "short_description": "90 NP - -",
    "unknown_encoding": "90 UNKNOWN - - - - BAD",
    "unknown_compact_flag": "90 NP - - - - BAD+j",
    "modrm_register_kind": "90 M Gb - - - BAD",
    "modreg_register_kind": "90 R Eb - - - BAD",
    "vex_register_kind": "VEX.NP.0f10 VM Rq Rq - - BAD",
    "immediate_register_kind": "90 I Rv - - - BAD",
    "width_four_operands": "0f10 RVMR Vx Hx Wx Lx BAD+w",
    "escape38_immediate": "0f3810 MI Eb Ib - - BAD",
    "escape3a_immediate": "0f3a10 NP - - - - BAD",
    "unescaped_prefix": "NP.90 NP - - - - BAD",
    "unescaped_vector_length": "L0.90 NP - - - - BAD",
    "unescaped_rexw": "W1.90 NP - - - - BAD",
    "unescaped_vex": "VEX.NP.90 NP - - - - BAD",
    "extended_vex": "VEX.NP.0f10+ NP - - - - BAD",
    "missing_vex_prefix": "VEX.0f10 NP - - - - BAD",
    "lig_dynamic_vector": "VEX.NP.LIG.0f10 RM Vx Wx - - BAD",
    "conflicting_dynamic_sizes": "VEX.NP.LIG.0f10 RVM Vx Bv Wx - BAD",
    "vsib_register": "0f10/r M Rx - - - BAD VSIB",
    "evex_vvvv_gp": "EVEX.NP.W0.0f10/r VM Bq Rq - - BAD",
    "evex_mask_destination": "EVEX.NP.W0.0f10/r MR Kq Kq - - BAD",
    "evex_missing_tuple": "EVEX.NP.W0.0f10 RM Vx Wx - - BAD",
    "evex_unknown_tuple": "EVEX.NP.W0.0f10 RM Vx Wx - - BAD TUPLE_UNKNOWN",
    "evex_missing_memory": "EVEX.NP.W0.0f10 NP - - - - BAD TUPLE_FULL_MEM",
    "evex_incompatible_broadcast": "EVEX.NP.W0.0f10 RM Vx Wx - - BAD+b TUPLE_FULL_MEM",
    "evex_bcst16_mismatch": "EVEX.NP.W0.0f10 RM Vx Wx - - BAD+b TUPLE_FULL_16",
    "evex_wrong_w": "EVEX.NP.W1.0f10 RM Vx Wx - - BAD+b TUPLE_FULL_32",
    "evex_wrong_memory_size": "EVEX.NP.W0.0f10 RM Vx Wq - - BAD TUPLE_FULL_MEM",
    "undocumented_invalid_before_filter": "NP.90 NP - - - - BAD UNDOC",
}


def write(path, contents):
    path.write_text(contents, encoding="utf-8")


def verify_sources(root, output):
    tree = json.loads((root / "tests/reference/tree.json").read_text())
    assert tree["sha"] == REVISION and not tree["truncated"]
    blobs = {item["path"]: item["sha"] for item in tree["tree"] if item["type"] == "blob"}
    paths = {"tests/reference/parseinstrs.py": "parseinstrs.py", "data/instrs.txt": "instrs.txt", "NOTICE": "LICENSE"}
    rows = ["path\tgit_blob_sha1\tsha256\tupstream_revision\n"]
    seen = set()
    with (root / "UPSTREAM.tsv").open() as source:
        for row in csv.DictReader(source, delimiter="\t"):
            path = row["path"]
            data = (root / path).read_bytes()
            sha256 = hashlib.sha256(data).hexdigest()
            blob = hashlib.sha1(b"blob " + str(len(data)).encode() + b"\0" + data).hexdigest()
            assert sha256 == row["sha256"], (path, "sha256", sha256)
            assert blob == row["git_blob_sha1"] == blobs[paths[path]], (path, "git blob", blob)
            assert path not in seen
            seen.add(path)
            rows.append(f"{path}\t{blob}\t{sha256}\t{REVISION}\n")
    assert seen == set(paths)
    write(output / "provenance.tsv", "".join(rows))


def load_reference(root):
    path = root / "tests/reference/parseinstrs.py"
    spec = importlib.util.spec_from_file_location("pinned_fadec_parseinstrs", path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def parse_source(module, source):
    entries = []
    forms = []
    for number, line in enumerate(source.splitlines(), start=1):
        if not line or line[0] == "#":
            continue
        weak = line[0] == "*"
        if weak:
            line = line[1:]
        opcode_string, desc_string = line.split(maxsplit=1)
        opcode = module.Opcode.parse(opcode_string)
        desc = module.InstrDesc.parse(desc_string)
        module.verifyOpcodeDesc(opcode, desc)
        entries.append((weak, opcode, desc))
        fields = ["SOURCE", number, int(weak), opcode_string, desc.encoding,
                  ",".join(op.regkind + op.sizestr for op in desc.operands), desc.mnemonic,
                  ",".join(sorted(desc.flags)), int("UNDOC" not in desc.flags),
                  int("O64" not in desc.flags), int("I64" not in desc.flags),
                  int("I64" not in desc.flags and not desc.mnemonic.startswith("RESERVED_"))]
        forms.append("\t".join(map(str, fields)) + "\n")
    return entries, "".join(forms)


def decode_semantics(public, private):
    sections = dict(re.findall(r"(?:#if|#elif) defined\(FD_DECODE_TABLE_(\w+)\)\n(.*?)(?=\n#(?:elif|else|endif)\b)", private, re.S))
    assert set(sections) == {"DATA", "DESCS", "STRTAB1", "STRTAB2", "STRTAB3", "DEFINES"}
    mnemonic_rows = re.findall(r"FD_MNEMONIC\(([^,]+),(\d+)\)\n", public)
    assert "".join(f"FD_MNEMONIC({name},{number})\n" for name, number in mnemonic_rows) == public
    ids = {name: int(number) for name, number in mnemonic_rows}
    assert len(ids) == len(mnemonic_rows)
    rows = []
    data = sections["DATA"].strip().rstrip(",").split(",")
    for index, word in enumerate(data):
        value = int(word, 16)
        assert 0 <= value <= 65535
        rows.append(f"TABLE\t{index}\t{value}\n")
    descs = re.findall(r"\{([^{}]*)\}", sections["DESCS"])
    assert ",".join("{" + desc + "}" for desc in descs) == sections["DESCS"].strip()
    for index, desc in enumerate(descs):
        if desc == "0":
            assert index == 0
            words = [0, 0, 0, 0]
        else:
            mnem, *words = [value.strip() for value in desc.split(",")]
            assert mnem.startswith("FDI_") and len(words) == 3
            words = [ids[mnem[4:]]] + [int(word) for word in words]
        rows.append(f"DESC\t{index}\t" + "\t".join(map(str, words)) + "\n")
    strings = ast.literal_eval(sections["STRTAB1"].strip())
    offsets = [int(value) for value in sections["STRTAB2"].strip().split(",")]
    lengths = [int(value) for value in sections["STRTAB3"].strip().split(",")]
    assert len(offsets) == len(lengths) == len(mnemonic_rows)
    for (name, number), offset, length in zip(mnemonic_rows, offsets, lengths):
        intel = strings[offset:offset + length]
        assert len(intel.encode()) == length
        rows.append(f"MNEM\t{number}\t{name}\t{intel}\t{offset}\t{length}\n")
    rows.append(f"STRTAB\t{strings}\n")
    defines = sections["DEFINES"].strip()
    roots = re.findall(r"#define FD_TABLE_OFFSET_(\d+) (\d+)", defines)
    assert roots
    assert "\n".join(f"#define FD_TABLE_OFFSET_{mode} {offset}" for mode, offset in roots) == defines
    for mode, offset in roots:
        rows.append(f"ROOT\t{mode}\t{offset}\n")
    return "".join(rows)


def encode_semantics(public, private):
    rows = []
    for line in public.splitlines():
        match = re.fullmatch(r"#define (FE_\w+) (0x[0-9a-f]+)", line)
        assert match, line
        rows.append(f"F\t{match[1]}\t{int(match[2], 16)}\n")
    for line in private.splitlines():
        match = re.fullmatch(r"\[(\d+)\] = (0x[0-9a-f]+),", line)
        assert match, line
        rows.append(f"A\t{match[1]}\t{int(match[2], 16)}\n")
    return "".join(sorted(rows))


def compact(text):
    return re.sub(r"\s+", "", text)


def encode2_semantics(public, private):
    rows = []
    signatures = {}
    for line in public.splitlines():
        if line.startswith("#define "):
            name = re.match(r"#define (fe64_\w+)\(", line)[1]
            rows.append(f"macro\t{name}\t{compact(line)}\n")
        else:
            name = re.match(r"unsigned \((fe64_\w+)\)", line)[1]
            assert name not in signatures and line.endswith(";")
            signatures[name] = compact(line[:-1])
    position = 0
    while position < len(private):
        match = re.match(r"unsigned \((fe64_\w+)\)[^{]+\{", private[position:])
        assert match, private[position:position + 100]
        assert signatures.pop(match[1]) == compact(match[0][:-1])
        start = position
        position += match.end()
        depth = 1
        while depth:
            assert position < len(private)
            depth += (private[position] == "{") - (private[position] == "}")
            position += 1
        rows.append(f"function\t{match[1]}\t{compact(private[start:position])}\n")
        while position < len(private) and private[position].isspace():
            position += 1
    assert not signatures
    return "".join(sorted(rows))


def invoke(module, mode, entries, modes, stats):
    capture = io.StringIO()
    with contextlib.redirect_stdout(capture):
        public, private = getattr(module, mode + "_table")(entries, SimpleNamespace(modes=modes, stats=stats))
    return public, private, capture.getvalue()


def profile(module, entries, forms, output, mode, modes, undoc):
    admitted = [entry for entry in entries if undoc or "UNDOC" not in entry[2].flags]
    name = mode + "-" + ("-".join(map(str, modes)) or "none") + "-" + str(int(undoc))
    target = output / name
    target.mkdir(exist_ok=True)
    public, private, quiet = invoke(module, mode, admitted, modes, False)
    with_stats = invoke(module, mode, admitted, modes, True)
    rerun = invoke(module, mode, admitted, modes, False)
    assert quiet == "" and (public, private) == with_stats[:2] == rerun[:2]
    assert bool(with_stats[2]) == (mode == "decode")
    script = Path(module.__file__).resolve()
    command = [sys.executable, str(script), *["--" + str(arch) for arch in modes]]
    if undoc:
        command.append("--with-undoc")
    command += ["--stats", mode, str(script.parents[2] / "data/instrs.txt"),
                str(target / "reference.cli.public"), str(target / "reference.cli.private")]
    cli = subprocess.run(command, capture_output=True, text=True)
    write(target / "reference.cli.stdout", cli.stdout)
    write(target / "reference.cli.stderr", cli.stderr)
    assert cli.returncode == 0 and cli.stderr == "", (name, cli.returncode, cli.stderr[:4000])
    assert (target / "reference.cli.public").read_text() == public
    assert (target / "reference.cli.private").read_text() == private
    assert cli.stdout == with_stats[2], name
    semantic = {"decode": decode_semantics, "encode": encode_semantics, "encode2": encode2_semantics}[mode](public, private)
    write(target / "reference.public", public)
    write(target / "reference.private", private)
    write(target / "reference.semantic.tsv", semantic)
    write(target / "reference.forms.tsv", forms)
    write(target / "reference.stats.txt", with_stats[2])
    counts = Counter(line.split("\t", 1)[0] for line in semantic.splitlines())
    if mode == "decode":
        assert counts["ROOT"] == len(modes), (name, "root count", counts["ROOT"], len(modes))
    write(target / "counts.tsv", "kind\tcount\n" + f"SOURCE\t{len(entries)}\nADMITTED\t{len(admitted)}\n" + "".join(f"{key}\t{counts[key]}\n" for key in sorted(counts)))
    return f"{name}\t{mode}\t{','.join(map(str, modes))}\t{int(undoc)}\t{len(entries)}\t{len(admitted)}\t{len(semantic.splitlines())}\n"


def negative_cases(module, output):
    target = output / "negative"
    target.mkdir(exist_ok=True)
    rows = ["case\treference_result\n"]
    control = "90 NP - - - - NOP\n"
    baseline = parse_source(module, control)
    for name, source in NEGATIVES.items():
        write(target / (name + ".txt"), source + "\n")
        try:
            parse_source(module, source)
        except Exception as error:
            rows.append(f"{name}\t{type(error).__name__}\n")
        else:
            raise AssertionError(("negative unexpectedly accepted", name))
        assert parse_source(module, control) == baseline, name
    write(target / "control.txt", control)
    write(target / "control.forms.tsv", baseline[1])
    for mode in ("decode", "encode", "encode2"):
        public, private, _ = invoke(module, mode, baseline[0], [64], False)
        semantic = {"decode": decode_semantics, "encode": encode_semantics, "encode2": encode2_semantics}[mode](public, private)
        write(target / ("control." + mode + ".semantic.tsv"), semantic)
    write(output / "negative.tsv", "".join(rows))


def pipeline_cases(module, output):
    target = output / "pipeline"
    target.mkdir(exist_ok=True)
    rows = ["case\tmode\tarch\tundoc\taccepted\n"]
    cases = [("duplicate32", "decode", [32, 32], False, "90 NP - - - - NOP", False),
             ("duplicate64", "decode", [64, 64], False, "90 NP - - - - NOP", False),
             ("duplicate_mixed", "decode", [64, 32, 64], False, "90 NP - - - - NOP", False)]
    for mode in ("decode", "encode", "encode2"):
        for undoc in (False, True):
            cases.append(("lazy_size_" + mode + "_" + str(int(undoc)), mode, [64], undoc,
                          "90 M Ebogus - - - BAD UNDOC", not undoc))
    for name, mode, modes, undoc, source, accepted in cases:
        source_path = target / (name + ".txt")
        write(source_path, source + "\n")
        public_path = target / (name + ".public")
        private_path = target / (name + ".private")
        command = [sys.executable, module.__file__, *["--" + str(arch) for arch in modes]]
        if undoc:
            command.append("--with-undoc")
        command += [mode, str(source_path), str(public_path), str(private_path)]
        cli = subprocess.run(command, capture_output=True, text=True)
        write(target / (name + ".stdout"), cli.stdout)
        write(target / (name + ".stderr"), cli.stderr)
        assert (cli.returncode == 0) == accepted, (name, cli.returncode, cli.stderr[:4000])
        if accepted:
            assert cli.stdout == cli.stderr == ""
            semantic = {"decode": decode_semantics, "encode": encode_semantics, "encode2": encode2_semantics}[mode]
            write(target / (name + ".semantic.tsv"), semantic(public_path.read_text(), private_path.read_text()))
        rows.append(f"{name}\t{mode}\t{','.join(map(str, modes))}\t{int(undoc)}\t{int(accepted)}\n")
    write(output / "pipeline.tsv", "".join(rows))


def main():
    assert sys.version_info >= (3, 9), "pinned Fadec oracle requires Python >= 3.9"
    args = argparse.ArgumentParser()
    args.add_argument("--out", required=True, type=Path)
    options = args.parse_args()
    output = options.out
    output.mkdir(parents=True, exist_ok=True)
    root = Path(__file__).resolve().parent.parent
    verify_sources(root, output)
    reference = root / "tests/reference/parseinstrs.py"
    calls = Counter()

    def record(frame, event, argument):
        if event == "call" and frame.f_code.co_filename == str(reference):
            calls[frame.f_code.co_firstlineno] += 1

    sys.setprofile(record)
    module = load_reference(root)
    source = (root / "data/instrs.txt").read_text()
    entries, forms = parse_source(module, source)
    assert len(entries) == 2337 and sum(entry[0] for entry in entries) == 13
    assert sum("UNDOC" in entry[2].flags for entry in entries) == 14
    rows = ["profile\tmode\tarch\tundoc\tsource\tadmitted\tsemantic_rows\n"]
    verdicts = ["mode\tarch\tundoc\tverdict\tdetail\n"]
    failures = []

    def attempt_profile(mode, modes, undoc):
        try:
            rows.append(profile(module, entries, forms, output, mode, modes, undoc))
            verdicts.append(f"{mode}\t{modes}\t{int(undoc)}\tPASS\tcomplete\n")
        except Exception as error:
            detail = re.sub(r"[\r\n\t]", " ", repr(error))
            failures.append((mode, modes, undoc, detail))
            verdicts.append(f"{mode}\t{modes}\t{int(undoc)}\tFAIL\t{detail}\n")
        write(output / "reference-profile-verdicts.tsv", "".join(verdicts))

    for mode in ("decode", "encode", "encode2"):
        for modes in ([32], [64], [32, 64]):
            for undoc in (False, True):
                attempt_profile(mode, modes, undoc)
    for mode, modes in (("decode", [64, 32]), ("encode", []), ("encode2", [])):
        for undoc in (False, True):
            attempt_profile(mode, modes, undoc)
    write(output / "profiles.tsv", "".join(rows))
    control_verdicts = ["stage\tverdict\tdetail\n"]
    for name, control in (("negative", negative_cases), ("pipeline", pipeline_cases)):
        try:
            control(module, output)
            control_verdicts.append(f"{name}\tPASS\tcomplete\n")
        except Exception as error:
            detail = re.sub(r"[\r\n\t]", " ", repr(error))
            failures.append((name, detail))
            control_verdicts.append(f"{name}\tFAIL\t{detail}\n")
        write(output / "reference-control-verdicts.tsv", "".join(control_verdicts))
    sys.setprofile(None)
    functions = []
    for node in ast.walk(ast.parse(reference.read_text())):
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            first = min([node.lineno] + [decorator.lineno for decorator in node.decorator_list])
            count = calls[first]
            functions.append((node.lineno, node.name, count))
    write(output / "function-coverage.tsv", "line\tfunction\tcalls\n" + "".join(f"{line}\t{name}\t{count}\n" for line, name, count in sorted(functions)))
    missing = [(line, name) for line, name, count in functions if not count]
    assert not failures, ("reference profile/control failures", failures)
    assert not missing, ("unexercised upstream functions", missing)
    print(f"FADEC_GEN_REFERENCE_PASS profiles=24 primary_profiles=18 boundary_profiles=6 cli_profiles=24 pipeline=9 source=2337 documented=2323 weak=13 negative={len(NEGATIVES)} functions={len(functions)} revision={REVISION}")


if __name__ == "__main__":
    main()

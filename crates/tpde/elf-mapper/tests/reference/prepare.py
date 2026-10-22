#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
# SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

import argparse
import hashlib
import json
import shutil
import subprocess
from pathlib import Path


def verify(root, revision):
    manifest = json.loads((root / "manifest.json").read_text())
    assert manifest["revision"] == revision, root
    for entry in manifest["files"]:
        value = (root / entry["path"]).read_bytes()
        assert hashlib.sha256(value).hexdigest() == entry["sha256"], entry["path"]
        assert hashlib.sha1(b"blob " + str(len(value)).encode() + b"\0" + value).hexdigest() == entry["git_blob"], entry["path"]
    return manifest


def run(command, output, label):
    (output / f"{label}.command.json").write_text(json.dumps(command) + "\n")
    with (output / f"{label}.stdout").open("wb") as stdout, (output / f"{label}.stderr").open("wb") as stderr:
        result = subprocess.run(command, stdout=stdout, stderr=stderr)
    if result.returncode:
        raise RuntimeError(f"{label}: {result.returncode}\n" + (output / f"{label}.stderr").read_text(errors="replace")[-12000:])


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    root = Path(__file__).resolve().parent
    tpde = root.parents[2]
    disarm = tpde / "disarm/reference"
    fadec = tpde / "fadec/tests/reference"
    inputs = {
        "tpde": verify(root, "116b2b83eb8491fd63c017285c91b007b8a26a67"),
        "disarm": verify(disarm, "2d13d3f410a52daff1c5d8ef07d623332f372560"),
        "fadec": verify(fadec, "3994d89500985491b1a7ccc17827a22058b3de49"),
    }
    for name, source, files in [
        ("disarm", disarm, ["disarm64.h", "encode.c"]),
        ("fadec", fadec, ["fadec-enc2.h", "encode2.c"]),
    ]:
        destination = output / name
        destination.mkdir(exist_ok=True)
        for filename in files:
            shutil.copyfile(source / filename, destination / filename)
    run(["python3", str(disarm / "parse.py"), "--features", "all", "--encode-in-header", "--feature-desc", str(disarm / "feat.txt"),
         str(output / "disarm/disarm64-public.inc"), str(output / "disarm/disarm64-private.inc"), str(disarm / "desc.txt")], output, "generate-disarm")
    run(["python3", str(fadec / "parseinstrs.py"), "--64", "--with-undoc", "encode2", str(fadec / "instrs.txt"),
         str(output / "fadec/fadec-encode2-public.inc"), str(output / "fadec/fadec-encode2-private.inc")], output, "generate-fadec")
    generated = []
    for path in sorted(output.glob("*/*")):
        if path.is_file():
            generated.append({"path": str(path.relative_to(output)), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()})
    (output / "reference-inputs.json").write_text(json.dumps({"inputs": inputs, "generated": generated}, indent=2) + "\n")
    print("TPDE_ELF_MAPPER_REFERENCE_PREPARE PASS tpde=116b2b83 disarm=all fadec=with-undoc")


if __name__ == "__main__":
    main()

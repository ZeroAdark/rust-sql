#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
# SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tempfile


SCHEMA = 1
RECEIPT = "reference-receipt.json"
REFERENCE = Path(__file__).resolve().parent
TPDE = REFERENCE.parents[2]
WORKSPACE = REFERENCE.parents[4]
# the qualification runner's tree (its profiles); the repository does not carry it
RUNNER = Path(os.environ["TPDE_RUNNER_DIR"]).resolve() if "TPDE_RUNNER_DIR" in os.environ else None
PINS = {
    "tpde": "116b2b83eb8491fd63c017285c91b007b8a26a67",
    "disarm": "2d13d3f410a52daff1c5d8ef07d623332f372560",
    "fadec": "3994d89500985491b1a7ccc17827a22058b3de49",
}
MODES = [f"{assertions}-{sanitizer}" for assertions in ["assert", "ndebug"] for sanitizer in ["native", "asan"]]
EXECUTABLES = [f"{arch}-{mode}" for mode in MODES for arch in ["a64", "x64"]]
OBJECTS = [f"{encoder}-{mode}.o" for mode in MODES for encoder in ["disarm", "fadec"]]
LIBRARIES = [f"unwind-{mode}.so" for mode in ["llvm", "gcc", "llvm-missing-remove"]]
GENERATED = [
    "disarm/disarm64.h", "disarm/encode.c", "disarm/disarm64-public.inc", "disarm/disarm64-private.inc",
    "fadec/fadec-enc2.h", "fadec/encode2.c", "fadec/fadec-encode2-public.inc", "fadec/fadec-encode2-private.inc",
]
PROVENANCE = [
    "prepare-command.txt", "generate-disarm.command.json", "generate-fadec.command.json", "reference-inputs.json",
    *[f"{label}-compile.txt" for label in EXECUTABLES],
    *[f"compile-{encoder}-{mode}-command.txt" for mode in MODES for encoder in ["disarm", "fadec"]],
    *[f"compile-unwind-{mode}-command.txt" for mode in ["llvm", "gcc", "llvm-missing-remove"]],
]


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def fingerprint(path, *, nonempty=True, elf=False, executable=False):
    require(path.is_file() and not path.is_symlink(), f"missing or symlinked file: {path}")
    digest = hashlib.sha256()
    with path.open("rb") as source:
        before = os.fstat(source.fileno())
        prefix = source.read(4)
        digest.update(prefix)
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
        after = os.fstat(source.fileno())
    require((before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns, before.st_ctime_ns) ==
            (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns, after.st_ctime_ns), f"file changed while hashing: {path}")
    require(not nonempty or after.st_size > 0, f"empty required file: {path}")
    require(not elf or prefix == b"\x7fELF", f"not an ELF artifact: {path}")
    require(not executable or os.access(path, os.X_OK), f"artifact is not executable: {path}")
    return {"sha256": digest.hexdigest(), "size": after.st_size}


def load_json(path):
    require(path.is_file() and not path.is_symlink(), f"missing or symlinked JSON: {path}")
    with path.open(encoding="utf-8") as source:
        return json.load(source)


def reference_sources():
    roots = {
        "tpde": REFERENCE,
        "disarm": TPDE / "disarm/reference",
        "fadec": TPDE / "fadec/tests/reference",
    }
    sources, manifests = {}, {}
    for name, root in roots.items():
        manifest_path = root / "manifest.json"
        manifest = load_json(manifest_path)
        require(manifest["revision"] == PINS[name], f"unexpected {name} revision")
        require(len(manifest["files"]) == {"tpde": 18, "disarm": 10, "fadec": 5}[name], f"unexpected {name} reference closure size")
        seen = set()
        for entry in manifest["files"]:
            relative = Path(entry["path"])
            require(not relative.is_absolute() and ".." not in relative.parts, f"invalid manifest path: {relative}")
            require(entry["path"] not in seen, f"duplicate manifest path: {relative}")
            seen.add(entry["path"])
            path = root / relative
            metadata = fingerprint(path)
            require(metadata["sha256"] == entry["sha256"], f"pinned source SHA mismatch: {path}")
            data = path.read_bytes()
            blob = b"blob " + str(len(data)).encode() + b"\0" + data
            require(hashlib.sha1(blob).hexdigest() == entry["git_blob"], f"pinned Git blob mismatch: {path}")
            sources[str(path.relative_to(WORKSPACE))] = metadata
        sources[str(manifest_path.relative_to(WORKSPACE))] = fingerprint(manifest_path)
        manifests[name] = manifest
    local_files = [path for path in REFERENCE.rglob("*") if path.is_file() and "__pycache__" not in path.parts and path.suffix != ".pyc"]
    local_files.extend([
        REFERENCE.parent / "common/mod.rs",
        REFERENCE.parent / "reference_prepare.rs",
        REFERENCE.parent / "upstream.rs",
        TPDE / "test-support/oracle.rs",
    ])
    require(RUNNER is not None, "TPDE_RUNNER_DIR must name the qualification runner's tree")
    local_files.extend([RUNNER / "profiles/tpde-component.json", RUNNER / "profiles/tpde-component.image.json"])
    for path in local_files:
        sources[os.path.relpath(path, WORKSPACE)] = fingerprint(path)
    return {"pins": PINS, "manifests": manifests, "files": dict(sorted(sources.items()))}


def loaded_libraries(path, ldd, identities):
    result = subprocess.run([ldd, str(path)], stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30, check=False)
    require(result.returncode == 0, f"cannot inspect loader dependencies: {path}: {result.stderr.decode(errors='replace')}")
    require(bool(result.stdout.strip()), f"empty loader dependency report: {path}")
    libraries = {}
    for line in result.stdout.decode("utf-8", errors="strict").splitlines():
        line = line.strip()
        if not line or line == "statically linked" or line.startswith(("linux-vdso.", "linux-gate.")):
            continue
        require("not found" not in line, f"missing loader dependency for {path}: {line}")
        if "=>" in line:
            name, location = [part.strip() for part in line.split("=>", 1)]
        else:
            name, location = "loader", line
        location = re.sub(r"\s+\(0x[0-9a-fA-F]+\)$", "", location)
        library = Path(location)
        require(library.is_absolute(), f"unrecognized loader dependency for {path}: {line}")
        resolved = str(library.resolve(strict=True))
        if resolved not in identities:
            identities[resolved] = fingerprint(Path(resolved))
        require(name not in libraries, f"duplicate loader dependency name for {path}: {name}")
        libraries[name] = {"resolved_path": resolved, "file": identities[resolved]}
    return dict(sorted(libraries.items()))


def toolchain(directory):
    effective = {
        "CXX": os.environ.get("CXX", "clang++"),
        "CC": os.environ.get("CC", os.environ.get("CXX", "clang")),
        "PYTHON": os.environ.get("PYTHON", "python3"),
        "GENERATOR_PYTHON": "python3",
        "LDD": "ldd",
    }
    tools, identities = {}, {}
    for name, command in effective.items():
        found = shutil.which(command)
        require(found is not None, f"tool unavailable: {name}={command!r}")
        resolved = Path(found).resolve(strict=True)
        key = str(resolved)
        if key not in identities:
            identities[key] = fingerprint(resolved, executable=True)
        result = subprocess.run([command, "--version"], stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30, check=False)
        require(result.returncode == 0, f"tool version failed: {name}={command!r}")
        require(bool(result.stdout or result.stderr), f"empty tool version: {name}={command!r}")
        tools[name] = {
            "command": command,
            "resolved_path": key,
            "executable": identities[key],
            "version_stdout": result.stdout.decode("utf-8", errors="strict"),
            "version_stderr": result.stderr.decode("utf-8", errors="strict"),
        }
    ldd = tools["LDD"]["command"]
    dependencies = {}
    dependency_cache = {}
    for name, tool in tools.items():
        if name == "LDD":
            continue
        resolved = tool["resolved_path"]
        if resolved not in dependency_cache:
            dependency_cache[resolved] = loaded_libraries(Path(resolved), ldd, identities)
        dependencies[f"tool:{name}"] = dependency_cache[resolved]
    for name in EXECUTABLES + LIBRARIES:
        dependencies[f"artifact:{name}"] = loaded_libraries(directory / name, ldd, identities)
    # Cargo changes its loader search paths; actual resolved library identities govern reuse.
    environment = {name: os.environ.get(name) for name in [
        "CXX", "CC", "PYTHON", "CPATH", "C_INCLUDE_PATH", "CPLUS_INCLUDE_PATH",
        "LIBRARY_PATH", "COMPILER_PATH", "GCC_EXEC_PREFIX", "SDKROOT", "LD_PRELOAD", "LD_AUDIT",
    ]}
    return {"system": platform.system(), "machine": platform.machine(), "environment": environment, "tools": tools, "loaded_libraries": dependencies}


def artifacts(directory, sources):
    files = {}
    for name in EXECUTABLES + OBJECTS + LIBRARIES + GENERATED + PROVENANCE:
        files[name] = fingerprint(directory / name, elf=name in EXECUTABLES + OBJECTS + LIBRARIES, executable=name in EXECUTABLES)
    generation = load_json(directory / "reference-inputs.json")
    require(generation["inputs"] == sources["manifests"], "generated reference input manifests do not match current source")
    expected = {name: files[name]["sha256"] for name in GENERATED}
    actual = {}
    for entry in generation["generated"]:
        require(entry["path"] not in actual, f"duplicate generated file: {entry['path']}")
        actual[entry["path"]] = entry["sha256"]
    require(actual == expected, "generated reference file hashes do not match generation provenance")
    for name in PROVENANCE:
        require(bool((directory / name).read_text(encoding="utf-8").strip()), f"blank compiler/generator provenance: {name}")
    return dict(sorted(files.items()))


def snapshot(directory):
    sources = reference_sources()
    return {
        "schema": SCHEMA,
        "directory": str(directory),
        "sources": sources,
        "toolchain": toolchain(directory),
        "artifacts": artifacts(directory, sources),
        "executables": EXECUTABLES,
        "libraries": LIBRARIES,
        "objects": OBJECTS,
    }


def atomic_write(path, value):
    descriptor, temporary = tempfile.mkstemp(prefix=".reference-receipt-", suffix=".tmp", dir=path.parent)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as output:
            json.dump(value, output, indent=2, sort_keys=True)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
        descriptor = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(descriptor)
        finally:
            os.close(descriptor)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def first_difference(recorded, current, path="receipt"):
    if type(recorded) is not type(current):
        return path
    if isinstance(current, dict):
        for key in sorted(set(recorded) | set(current)):
            if key not in recorded or key not in current:
                return f"{path}.{key}"
            if mismatch := first_difference(recorded[key], current[key], f"{path}.{key}"):
                return mismatch
        return None
    return path if recorded != current else None


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--directory", required=True, type=Path)
    parser.add_argument("--mode", required=True, choices=["write", "verify"])
    args = parser.parse_args()
    directory = args.directory.resolve(strict=True)
    require(directory.is_dir(), f"reference cache is not a directory: {directory}")
    receipt = directory / RECEIPT
    if args.mode == "verify":
        recorded = load_json(receipt)
        require(recorded.get("schema") == SCHEMA, "unsupported reference receipt schema")
        current = snapshot(directory)
        mismatch = first_difference(recorded, current)
        require(mismatch is None, f"stale or corrupt reference receipt: {mismatch}")
    else:
        atomic_write(receipt, snapshot(directory))
    print(f"TPDE_ELF_MAPPER_REFERENCE_RECEIPT PASS mode={args.mode} executables=8 libraries=3 objects=8")


if __name__ == "__main__":
    main()

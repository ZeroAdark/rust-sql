#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2024 Alexis Engelke
# SPDX-License-Identifier: BSD-3-Clause

import argparse
from configparser import ConfigParser
import contextlib
import hashlib
import io
import json
from pathlib import Path
import runpy
import struct
import sys

REFERENCE = Path(__file__).resolve().parent.parent / "reference"


def verify_sources():
    manifest = json.loads((REFERENCE / "manifest.json").read_text())
    assert manifest["revision"] == "2d13d3f410a52daff1c5d8ef07d623332f372560"
    for entry in manifest["files"]:
        data = (REFERENCE / entry["path"]).read_bytes()
        assert hashlib.sha256(data).hexdigest() == entry["sha256"], entry["path"]
        blob = b"blob " + str(len(data)).encode() + b"\0" + data
        assert hashlib.sha1(blob).hexdigest() == entry["git_blob"], entry["path"]


def profile_names():
    config = ConfigParser(interpolation=None, delimiters=(":",), strict=True)
    config.optionxform = str
    config.read(REFERENCE / "feat.txt")
    names = ["default", "all", "really-all"]
    names.extend(f"armv8.{i}a" for i in range(10))
    names.extend(f"armv9.{i}a" for i in range(5))
    names.extend(name.lower() for name in config.sections())
    names.extend(config[name]["avail"] for name in config if "avail" in config[name])
    names.extend(["aes,pmull", "fp16,fcma", "armv8.1a,sha3", " all "])
    return list(dict.fromkeys(names))


def snapshot(ns):
    groups, forms = [], []
    for name in ns["config"].sections():
        props = ns["config"][name]
        desc = ns["parse_desc"](props["desc"])
        groups.append({"name": name, "properties": list(props.items()), "desc": desc})
        for key in props:
            if not key.startswith("encmnem"):
                continue
            suffix = key[7:]
            fmt = ns["Fmt"]
            operands = fmt(props[f"encops{suffix}"], desc)
            condition = fmt(props.get(f"enccond{suffix}", ""), desc)
            feature = fmt(props.get(f"encfeat{suffix}", props.get("feat", "")), desc)
            for form, masks in fmt(props[key], desc).expansions().items():
                features = feature.eval(masks)
                forms.append({
                    "group": name, "property": key, "name": form, "masks": masks,
                    "operands": operands.eval(masks), "condition": condition.eval(masks),
                    "features": features,
                    "enabled": ns["check_feature"](ns["features"], features),
                })
    decoder, encoder = ns["decoder"], ns["encoder"]
    return {
        "features": list(ns["features"].items()),
        "groups": groups,
        "group_numbers": list(decoder.grpnums.items()),
        "mnemonics": list(decoder.mnems.items()),
        "trie_descriptions": decoder.trie_descs,
        "decode_strings": list(decoder.decstr.items()),
        "operand_decoders": list(decoder.decoders.items()),
        "encoder_forms": forms,
        "encoder_declarations": encoder.decls,
        "encoder_definitions": encoder.defns,
    }


def generate(args):
    args.output.mkdir(parents=True, exist_ok=True)
    argv = [str(REFERENCE / "parse.py")]
    if args.features != "default":
        argv.extend(["--features", args.features])
    if not args.no_feature_desc:
        argv.extend(["--feature-desc", str(args.feature_desc or REFERENCE / "feat.txt")])
    if args.coverage:
        argv.append("--coverage")
    if args.encode_in_header:
        argv.append("--encode-in-header")
    argv.extend([str(args.output / "public.inc"), str(args.output / "private.inc")])
    argv.extend(map(str, args.desc or [REFERENCE / "desc.txt"]))
    diagnostics, previous = io.StringIO(), sys.argv
    try:
        sys.argv = argv
        with contextlib.redirect_stdout(diagnostics):
            ns = runpy.run_path(str(REFERENCE / "parse.py"), run_name="__main__")
            ns["args"].out_public.close()
            ns["args"].out_private.close()
            decoder = ns["decoder"]
            rows = [f"feature\t{name}\t{int(enabled)}\n" for name, enabled in ns["features"].items()]
            rows.extend(f"group\t{name}\t{number}\n" for name, number in decoder.grpnums.items())
            rows.extend(f"mnemonic\t{name}\t{number}\n" for name, number in decoder.mnems.items())
            rows.extend(
                f"trie_desc\t{desc.fixed.val:x}\t{desc.fixed.mask:x}\t{desc.subgrp}\t{desc.name}\n"
                for desc in decoder.trie_descs
            )
            rows.extend(f"decstr\t{number}\t{text}\n" for number, text in decoder.decstr.items())
            model = snapshot(ns)
            rows.extend(
                f"form\t{form['group']}\t{form['property'][7:]}\t{form['name']}\t"
                f"{form['masks'][0].val:#x}\t{form['masks'][0].mask:#x}\t{int(form['enabled'])}\t"
                f"{form['features']}\t{form['operands']}\t{form['condition']}\n"
                for form in model["encoder_forms"]
            )
            (args.output / "manifest.tsv").write_text("".join(rows))
            if args.classifications:
                words = [0, 0xffffffff]
                for desc in decoder.trie_descs:
                    words.extend([desc.fixed.val, desc.fixed.val | (desc.fixed.mask ^ 0xffffffff)])
                    words.extend(desc.fixed.val ^ (1 << bit) for bit in range(32))
                state = 0x13579bdf
                for _ in range(4096):
                    state ^= (state << 13) & 0xffffffff
                    state ^= state >> 17
                    state ^= (state << 5) & 0xffffffff
                    words.append(state)
                (args.output / "classification.bin").write_bytes(
                    b"".join(struct.pack("<I", word) for word in words)
                )
            if args.model:
                (args.output / "model.json").write_text(
                    json.dumps(model, separators=(",", ":"), ensure_ascii=True) + "\n"
                )
    finally:
        sys.argv = previous
        (args.output / "diagnostics.txt").write_text(diagnostics.getvalue())


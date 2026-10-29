#!/usr/bin/env python3
"""Interleave fresh-cluster baseline/candidate runs; keep reports and reproducibility metadata."""
import argparse
import hashlib
import json
import platform
from pathlib import Path
import subprocess
import sys

ap = argparse.ArgumentParser()
ap.add_argument("--baseline", type=Path, required=True)
ap.add_argument("--candidate", type=Path, required=True)
ap.add_argument("--output-dir", type=Path, required=True)
ap.add_argument("--work-dir", type=Path, required=True, help="short absolute Linux path for retained clusters")
ap.add_argument("--repeats", type=int, default=5)
ap.add_argument("--seconds", type=int, default=10)
args = ap.parse_args()
if args.repeats < 1 or args.seconds < 1:
    ap.error("positive repeats and seconds required")
args.output_dir.mkdir(parents=True, exist_ok=True)
metadata = {"platform": platform.platform(), "repeats": args.repeats, "seconds": args.seconds,
            "durability": "fsync=on, synchronous_commit=on", "server_binaries": {}}
for label in ("baseline", "candidate"):
    path = getattr(args, label).resolve() / "postgres"
    metadata["server_binaries"][label] = {"path": str(path), "sha256": hashlib.file_digest(path.open("rb"), "sha256").hexdigest()}
(args.output_dir / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
for i in range(args.repeats):
    for label in (("baseline", "candidate") if i % 2 == 0 else ("candidate", "baseline")):
        out = args.output_dir / f"{label}-{i + 1}.json"
        # Refuse to overwrite evidence: use a fresh output directory for another experiment.
        if out.exists():
            raise FileExistsError(out)
        cmd = [sys.executable, str(Path(__file__).with_name("server_smoke.py")), "--bin-dir", str(getattr(args, label)),
               "--output", str(out), "--work-dir", str(args.work_dir), "--mixed-seconds", str(args.seconds)]
        result = subprocess.run(cmd, capture_output=True, text=True, timeout=args.seconds + 240)
        if result.returncode:
            print(result.stdout, result.stderr, file=sys.stderr)
            raise SystemExit(result.returncode)
        report = json.loads(out.read_text())["mixed"]
        print(label, i + 1, json.dumps(report), flush=True)

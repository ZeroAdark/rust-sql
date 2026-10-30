#!/usr/bin/env python3
"""Run unmodified PostgreSQL regression fixtures against a native control server."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess

from cluster import Cluster


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, required=True)
    parser.add_argument("--regression-dir", type=Path, required=True)
    parser.add_argument("--source-dir", type=Path, help="regression source directory for an out-of-tree build")
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--reference-patch", type=Path, help="identify the patch used for a separate control build")
    args = parser.parse_args()
    report = {"passed": False, "profile": "patched-native-postgresql" if args.reference_patch else "native-postgresql-raw"}
    if args.reference_patch:
        report["reference_patch_sha256"] = hashlib.sha256(args.reference_patch.read_bytes()).hexdigest()
    try:
        with Cluster(args.reference, args.work_dir, reference=True) as pg:
            report.update(cluster=str(pg.root), version=pg.version)
            with (args.reference / "postgres").open("rb") as binary:
                report["binary_sha256"] = hashlib.file_digest(binary, "sha256").hexdigest()
            build = args.regression_dir.resolve()
            source = args.source_dir.resolve() if args.source_dir else build
            output = pg.root / "regression"
            output.mkdir()
            command = [str(build / "pg_regress"), f"--inputdir={source}", f"--outputdir={output}",
                       f"--bindir={args.reference.resolve()}", f"--dlpath={build}",
                       f"--host={pg.socket}", "--port=55440", "--user=rustsql", "--dbname=regression",
                       f"--schedule={source / 'parallel_schedule'}"]
            with (pg.root / "regression.log").open("w") as log:
                result = subprocess.run(command, cwd=build, env=pg.env, stdout=log, stderr=log, timeout=900)
            report.update(returncode=result.returncode, results=str(output), passed=result.returncode == 0)
    except Exception as error:
        report["error"] = f"{type(error).__name__}: {error}"
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())

# SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
# SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

import os
from pathlib import Path
import subprocess

root = Path(__file__).resolve().parents[4]
subprocess.run([
    'python3', str(Path(os.environ['TPDE_RUNNER_DIR']) / 'run.py'), '--profile', 'tpde-component',
    '--crate', 'tpde_string_table', '--output',
    os.environ.get('COMPONENT_RESULTS', '/work/results'),
], check=True)

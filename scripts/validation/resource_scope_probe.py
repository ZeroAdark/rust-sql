#!/usr/bin/env python3
"""Live controls for CPU attribution, memory sampling and intentional OOM detection."""
import argparse
import json
from pathlib import Path
import subprocess
import sys
import time

from resource_scope import DelegatedScope, Sampler, delta, snapshot


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    scope = DelegatedScope([0, 2, 4, 6], [8, 10])
    group, launcher = scope.server(512)
    report = {'passed': False, 'checks': []}
    try:
        burn = 'import time; end=time.process_time()+0.25\nwhile time.process_time()<end: pass'
        program = f'import subprocess,sys\nexec({burn!r})\nsubprocess.run([sys.executable,"-c",{burn!r}],check=True)'
        before = snapshot(group)
        subprocess.run(launcher + [sys.executable, '-c', program], check=True)
        after = snapshot(group)
        cpu = delta(before['cpu'], after['cpu'])
        assert cpu['usage_usec'] >= 500000 and not (group / 'cgroup.procs').read_text().strip()
        report['checks'].append({'name': 'exited_parent_and_child_cpu_retained', 'cpu_delta': cpu})
        before_server, before_driver = snapshot(group), snapshot(scope.driver)
        exec(burn)
        server_delta = delta(before_server['cpu'], snapshot(group)['cpu'])
        driver_delta = delta(before_driver['cpu'], snapshot(scope.driver)['cpu'])
        assert server_delta['usage_usec'] == 0 and driver_delta['usage_usec'] >= 250000
        report['checks'].append({'name': 'client_cpu_excluded', 'server_cpu_delta': server_delta, 'driver_cpu_delta': driver_delta})
        sampler = Sampler(group)
        sampler.start()
        try:
            subprocess.run(launcher + [sys.executable, '-c', 'import time; a=bytearray(128*1024**2)\nfor i in range(0,len(a),4096): a[i]=1\ntime.sleep(1.2)'], check=True)
        finally:
            sampled = sampler.stop()
        assert sampled['sampled_memory_current_peak_bytes'] >= 128 * 1024 ** 2
        assert sampled['sampled_summed_pss_peak_bytes'] >= 128 * 1024 ** 2
        report['checks'].append({'name': 'resident_allocation_sampled', **sampled})
        # Only this disposable, empty leaf is reduced. The controller and every
        # SQL server are outside this intentional allocation-failure control.
        (group / 'memory.max').write_text(str(32 * 1024 ** 2))
        before = snapshot(group)
        killed = subprocess.run(launcher + [sys.executable, '-c', 'a=bytearray(128*1024**2)'], capture_output=True, text=True)
        after = snapshot(group)
        events = delta(before['events'], after['events'])
        assert killed.returncode == -9 and events['oom_kill'] >= 1 and events['oom_group_kill'] >= 1
        assert after['memory_swap_current'] == 0
        report['checks'].append({'name': 'intentional_oom_detected', 'returncode': killed.returncode, 'events': events})
        report['passed'] = True
    finally:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2) + '\n')
        assert not (group / 'cgroup.procs').read_text().strip()
        group.rmdir()
    print(json.dumps(report), flush=True)


if __name__ == '__main__':
    main()

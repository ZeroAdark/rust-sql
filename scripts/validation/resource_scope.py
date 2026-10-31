"""Server-only cgroup v2 accounting inside an explicitly delegated private unit.

Run the controller as an ordinary user in a rustsql-qualification-*.service with
Delegate=cpu memory pids. It moves itself/clients into a driver child and creates
a fresh server child per trial. No host/root cgroup configuration is changed.
"""
import os
from pathlib import Path
import sys
import threading
import time


CGROUP = Path('/sys/fs/cgroup')


def current_group():
    line = Path('/proc/self/cgroup').read_text().strip()
    if not line.startswith('0::/') or '\n' in line:
        raise ValueError('unified cgroup v2 required')
    return (CGROUP / line[4:]).resolve()


def private_unit(path):
    return (path.parent == CGROUP / 'system.slice' and
            path.name.startswith('rustsql-qualification-') and path.name.endswith('.service'))


def counters(path):
    return {key: int(value) for key, value in (line.split() for line in path.read_text().splitlines())}


def snapshot(group):
    return {'cpu': counters(group / 'cpu.stat'), 'events': counters(group / 'memory.events'),
            'memory': counters(group / 'memory.stat'),
            **{name.replace('.', '_'): int((group / name).read_text())
               for name in ('memory.current', 'memory.peak', 'memory.swap.current')}}


def delta(before, after):
    result = {key: after.get(key, 0) - before.get(key, 0) for key in before.keys() | after.keys()}
    if any(value < 0 for value in result.values()):
        raise ValueError('cumulative counter decreased')
    return result


class DelegatedScope:
    def __init__(self, server_cpus, client_cpus):
        self.root = current_group()
        if os.geteuid() == 0 or not private_unit(self.root) or self.root.stat().st_uid != os.geteuid():
            raise ValueError('ordinary-user private delegated systemd unit required')
        if {int(pid) for pid in (self.root / 'cgroup.procs').read_text().split()} != {os.getpid()}:
            raise ValueError('delegated unit is not exclusive to this controller')
        allowed = os.sched_getaffinity(0)
        self.server_cpus, self.client_cpus = set(server_cpus), set(client_cpus)
        if (not self.server_cpus or not self.client_cpus or self.server_cpus & self.client_cpus
                or not (self.server_cpus | self.client_cpus) <= allowed):
            raise ValueError('disjoint, available server/client CPUs required')
        self.driver = self.root / 'driver'
        self.driver.mkdir()
        (self.driver / 'cgroup.procs').write_text(str(os.getpid()))
        (self.root / 'cgroup.subtree_control').write_text('+cpu +memory +pids')
        for name, value in {'cpu.max': f'{100000 * len(self.client_cpus)} 100000',
                            'memory.max': str(2 * 1024 ** 3), 'memory.swap.max': '0', 'pids.max': '128'}.items():
            (self.driver / name).write_text(value)
        os.sched_setaffinity(0, self.client_cpus)
        self.sequence = 0

    def server(self, memory_mib):
        if not 512 <= memory_mib <= 8192:
            raise ValueError('server memory limit must be between 512 and 8192 MiB')
        self.sequence += 1
        group = self.root / f'server-{self.sequence:04}'
        group.mkdir()
        for name, value in {'cpu.max': f'{100000 * len(self.server_cpus)} 100000',
                            'memory.max': str(memory_mib * 1024 ** 2), 'memory.swap.max': '0',
                            'memory.oom.group': '1', 'pids.max': '1024'}.items():
            (group / name).write_text(value)
        launcher = [sys.executable, str(Path(__file__).resolve()), str(group),
                    ','.join(map(str, sorted(self.server_cpus))), '--']
        return group, launcher


class Sampler:
    def __init__(self, group):
        self.group = group
        self.done = threading.Event()
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.current_peak = self.rss_peak = self.pss_peak = self.samples = self.incomplete = 0
        self.errors = []

    def run(self):
        next_process_sample = 0
        while not self.done.is_set():
            try:
                self.current_peak = max(self.current_peak, int((self.group / 'memory.current').read_text()))
                self.samples += 1
                if time.monotonic() >= next_process_sample:
                    next_process_sample = time.monotonic() + 0.5
                    rss = pss = 0
                    pids = {int(pid) for pid in (self.group / 'cgroup.procs').read_text().split()}
                    try:
                        for pid in pids:
                            fields = dict(line.split(':', 1) for line in Path(f'/proc/{pid}/smaps_rollup').read_text().splitlines()[1:])
                            rss += int(fields['Rss'].split()[0]) * 1024
                            pss += int(fields['Pss'].split()[0]) * 1024
                        self.rss_peak, self.pss_peak = max(self.rss_peak, rss), max(self.pss_peak, pss)
                    except (FileNotFoundError, ProcessLookupError):
                        self.incomplete += 1
            except Exception as error:
                self.errors.append(repr(error))
                return
            self.done.wait(0.05)

    def start(self):
        self.thread.start()

    def stop(self):
        self.done.set()
        self.thread.join(timeout=5)
        if self.thread.is_alive() or self.errors:
            raise RuntimeError(f'resource sampler failed: {self.errors}')
        return {'samples': self.samples, 'incomplete_process_samples': self.incomplete,
                'sampled_memory_current_peak_bytes': self.current_peak,
                'sampled_summed_rss_peak_bytes': self.rss_peak, 'sampled_summed_pss_peak_bytes': self.pss_peak}


def launch(arguments):
    group, cpus, separator, *command = arguments
    group = Path(group).resolve()
    current = current_group()
    if (separator != '--' or not command or current.name != 'driver' or group.parent != current.parent
            or not private_unit(group.parent) or not group.name.startswith('server-')
            or group.stat().st_uid != os.geteuid()):
        raise ValueError('server launcher requires a sibling inside the owned delegated unit')
    (group / 'cgroup.procs').write_text(str(os.getpid()))
    os.sched_setaffinity(0, {int(cpu) for cpu in cpus.split(',')})
    if current_group() != group:
        raise RuntimeError('server did not enter the requested cgroup')
    os.execvpe(command[0], command, os.environ)


if __name__ == '__main__':
    launch(sys.argv[1:])

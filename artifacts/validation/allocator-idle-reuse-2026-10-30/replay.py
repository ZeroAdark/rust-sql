#!/usr/bin/env python3
"""Verify archived bytes and replay comparisons and the final SQL audit; no server starts."""
import hashlib
import importlib.util
import json
import locale
from pathlib import Path, PurePosixPath
import shutil
import subprocess
import sys
import tarfile
import tempfile
from zoneinfo import reset_tzpath


def require(condition, message):
    if not condition:
        raise ValueError(message)


def read(path):
    return json.loads(path.read_text())


def unpack(artifact, record, destination):
    archive = artifact / record['file']
    data = archive.read_bytes()
    require(len(data) == record['bytes'], 'archive size: ' + record['file'])
    require(hashlib.sha256(data).hexdigest() == record['sha256'], 'archive hash: ' + record['file'])
    with tarfile.open(archive, 'r:gz') as stream:
        members = stream.getmembers()
        names = [member.name for member in members]
        require(len(names) == len(set(names)) and set(names) == set(record['members']), 'archive member set')
        for member in members:
            path = PurePosixPath(member.name)
            require(member.isfile() and not path.is_absolute() and '..' not in path.parts
                    and '\\' not in member.name, 'unsafe archive member')
            expected = record['members'][member.name]
            require(member.size == expected['bytes'], 'member size: ' + member.name)
            content = stream.extractfile(member).read()
            require(hashlib.sha256(content).hexdigest() == expected['sha256'], 'member hash: ' + member.name)
            target = destination.joinpath(*path.parts)
            require(target.resolve().is_relative_to(destination.resolve()), 'member escaped destination')
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(content)
    return len(members)


def replay_command(command, expected, output):
    subprocess.run([sys.executable, *map(str, command), '--output', str(output)],
                   check=True, stdout=subprocess.PIPE, text=True)
    require(read(output) == read(expected), 'comparison changed: ' + str(expected))


def main():
    if locale.getpreferredencoding(False).replace('-', '').lower() != 'utf8':
        subprocess.run([sys.executable, '-X', 'utf8', str(Path(__file__).resolve())], check=True)
        return
    artifact = Path(__file__).resolve().parent
    manifest = read(artifact / 'manifest.json')
    with tempfile.TemporaryDirectory(prefix='rust-sql-idle-replay-') as temporary:
        root = Path(temporary).resolve()
        require(root.is_relative_to(Path(tempfile.gettempdir()).resolve()), 'unexpected temporary directory')
        extracted = {}
        total = 0
        for record in manifest['archives']:
            destination = root / record['file'].removesuffix('.tar.gz')
            total += unpack(artifact, record, destination)
            extracted[record['file']] = destination
        final = extracted['evidence.tar.gz']
        reset_tzpath([str(final / 'reference/timezones')])
        experiments = extracted['experiments.tar.gz']
        followups = extracted['followups.tar.gz']
        cold = extracted['cold.tar.gz']
        workspace = root / 'audit-workspace'
        sources = workspace / 'scripts/validation'
        shutil.copytree(final / 'validation-source', sources)
        shutil.copytree(final / 'validation-support', workspace / 'scripts', dirs_exist_ok=True)
        shutil.copytree(final / 'reference/fixtures', workspace / 'crates/postgres-18.6-reference/src/test/regress')
        shutil.copytree(final / 'policy', workspace / 'regress')

        jobs = [(experiments, 'idle-handoff', 'calibration', True),
                (experiments, 'idle-reuse', 'calibration', True),
                (experiments, 'idle-reuse', 'matrix', False),
                (followups, 'idle-reclaim', 'matrix', False),
                (followups, 'idle-boundary', 'matrix', False),
                (cold, 'idle-cold', 'matrix', False),
                (final, 'idle-safe-final', 'matrix', False)]
        for index, (archive, stem, kind, calibration) in enumerate(jobs):
            stage = archive / stem
            flags = ['--calibration'] if calibration else []
            replay_command([sources / 'compare_resource.py', *flags,
                            '--input-dir', stage / f'{stem}-{kind}-20261030',
                            '--source-dir', stage / f'{stem}-calibration-source-20261030'],
                           stage / 'reports' / f'{stem}-{kind}-comparison-20261030.json',
                           root / f'mixed-{index}.json')
        for index, (archive, stem, suffix) in enumerate([
                (experiments, 'idle-reuse', 'calibration2'),
                (followups, 'idle-reclaim', 'calibration'),
                (followups, 'idle-boundary', 'calibration'),
                (cold, 'idle-cold', 'calibration'),
                (final, 'idle-safe-final', 'calibration')]):
            stage = archive / stem
            replay_command([stage / 'drivers' / ('compare_' + stem.replace('-', '_') + '_connections.py'),
                            '--input-dir', stage / f'{stem}-connection-{suffix}-20261030'],
                           stage / 'reports' / f'{stem}-connection-comparison-20261030.json',
                           root / f'connections-{index}.json')

        # Policy modules resolve their checked-in policies relative to scripts/validation.
        sys.path.insert(0, str(sources))
        spec = importlib.util.spec_from_file_location('idle_evidence_audit', sources / 'engine_regression_audit.py')
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        stage = final / 'idle-safe-final'
        audit = module.audit(final / 'reference/fixtures', final / 'reference/results',
                             stage / 'sql/regression/results', workspace / 'regress/engine-profile',
                             root / 'audit-replayed')
        recorded = read(stage / 'sql/audit/summary.json')
        changes = [{'case': actual['case'], 'error': actual.get('error'),
                    'changed_fields': sorted(key for key in actual.keys() | expected.keys()
                                             if actual.get(key) != expected.get(key))}
                   for actual, expected in zip(audit['cases'], recorded['cases']) if actual != expected]
        require(all(value == recorded[key] for key, value in audit.items()),
                'SQL audit changed: ' + json.dumps(changes, ensure_ascii=True))
        require(audit['passed'] and audit['accepted'] == audit['total'] == 231, 'SQL audit incomplete')
        require(read(artifact / 'mixed-comparison.json') == read(root / 'mixed-6.json'), 'published mixed summary')
        require(read(artifact / 'connection-comparison.json') == read(root / 'connections-4.json'), 'published connection summary')
        print(json.dumps({'verified_archive_members': total, 'mixed_comparisons': len(jobs),
                          'connection_comparisons': 5, 'reviewed_sql_cases': audit['accepted'],
                          'server_started': False, 'production_qualified': False}))


if __name__ == '__main__':
    main()

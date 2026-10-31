"""Independently reconstruct completed granular-budget phases, retaining any failed guard."""
from pathlib import Path
import argparse
import hashlib
import json

from review_known_budget_performance import BINARIES, review_phase, tamper_controls

if not __debug__:
    raise RuntimeError('Assertions must remain enabled')


def review(read, allow_partial=False):
    build = json.loads(read('granular-budget-build-20261031/report.json'))
    assert build['passed'] and not build['promoted']
    assert build['base_commit'] == 'b3d1a667e200da873aa72fda7530cce5c2c9279f'
    assert len(build['source_sha256']) == 7
    for name, sha in build['source_sha256'].items():
        assert hashlib.sha256(read('granular-budget-build-20261031/sources/' + name)).hexdigest() == sha
    binary = build['binaries']['postgres']['sha256']
    binaries = BINARIES | {'rustsql': binary}
    screen = json.loads(read('granular-budget-screen-20261031.json'))
    assert not screen['promoted'] and screen['pressure_passed']
    phases = {}
    expected_order = ['medium-1g', 'medium', 'capacity']
    for stage in screen['stages']:
        assert stage['exit'] == 0
        if stage['script'] == 'run_granular_budget_pressure.py':
            assert not stage['arguments'] and not phases
            continue
        assert stage['script'] == 'run_granular_budget_performance.py'
        args = stage['arguments']
        key = args[0] + ('-1g' if args == ['medium', '1024'] else '')
        assert key == expected_order[len(phases)]
        phases[key] = review_phase(read, key, campaign='granular-budget', binaries=binaries)
        for field in ('phase', 'engines', 'candidate_change_percent', 'preservation_failures', 'evidence_valid', 'passed'):
            assert screen[key][field] == phases[key][field]
    assert phases
    failures = [key for key, result in phases.items() if not result['passed']]
    if failures:
        assert failures == [list(phases)[-1]]
        assert screen['rejected_at'] == failures[0] and not screen['passed']
    else:
        if list(phases) == expected_order:
            assert screen['passed']
        else:
            assert allow_partial and not screen['passed']
    rejected = tamper_controls(read, 'medium-1g', campaign='granular-budget', binaries=binaries)
    return {'evidence_valid': True, 'recorded_evidence_only': True, 'candidate_promoted': False,
            'candidate_sha256': binary, 'phases': phases,
            'complete': bool(failures) or list(phases) == expected_order,
            'completed_phases_passed': not failures,
            'preservation_passed': not failures and list(phases) == expected_order,
            'timed_statements': sum(p['timed_statements'] for p in phases.values()),
            'warmup_statements': sum(p['warmup_statements'] for p in phases.values()),
            'tamper_controls_rejected': rejected}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--base', type=Path, default=Path('/home/rustsql'))
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--allow-partial', action='store_true')
    args = parser.parse_args()
    assert not args.output.exists()
    result = review(lambda name: (args.base / name).read_bytes(), allow_partial=args.allow_partial)
    args.output.write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result))


if __name__ == '__main__':
    main()

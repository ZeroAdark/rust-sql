#!/usr/bin/env python3
"""Validate complete fixed-work evidence and report paired resource distributions."""
import argparse
import hashlib
import json
import math
from pathlib import Path
import re
import statistics

from fixed_workload import PER_BUNDLE, SQL, expected_updates


def require(condition, message):
    if not condition:
        raise ValueError(message)


def distribution(values):
    require(bool(values) and all(math.isfinite(value) and value > 0 for value in values), 'invalid metric')
    return {'median': statistics.median(values), 'min': min(values), 'max': max(values), 'values': values}


def comparison(baseline, candidate):
    require(len(baseline) == len(candidate), 'unpaired observations')
    first, second = distribution(baseline), distribution(candidate)
    changes = [(after / before - 1) * 100 for before, after in zip(baseline, candidate)]
    return {'baseline': first, 'candidate': second,
            'median_change_pct': (second['median'] / first['median'] - 1) * 100,
            'paired_change_pct': {'values': changes, 'median': statistics.median(changes),
                                  'min': min(changes), 'max': max(changes)}}


def validate_trial(report, metadata, identity):
    label, memory, repetition = identity
    require((report['label'], report['memory_mib'], report['repetition']) == identity, 'trial identity mismatch')
    require(report['passed'] is True and not report.get('error') and not report.get('cleanup_error')
            and report['remaining_server_pids'] == [], 'failed trial or live server')
    expected_limits = {'cpu.max': f"{len(set(metadata['server_cpus'])) * 100000} 100000",
                       'memory.max': str(memory * 1024 ** 2), 'memory.swap.max': '0', 'pids.max': '1024'}
    require(report['limits'] == expected_limits, 'resource limits differ')
    settings = {name: setting for name, setting, _ in report['settings']}
    # Older fixed-work captures used a hard-coded limit of four. New captures
    # record the chosen limit and must match it in every individual trial.
    active_limit = metadata.get('max_active_queries', 4)
    require(type(active_limit) is int and 1 <= active_limit <= 16, 'invalid admission limit')
    require(settings['fsync'] == 'on' and settings['synchronous_commit'] == 'on'
            and settings['max_active_queries'] == str(active_limit) and settings['work_mem'] == '16384', 'durability or workload settings differ')
    measurement = report['measurement']
    require(measurement['passed'] is True, 'failed measurement')
    for recorded, argument in (('clients', 'clients'), ('bundles_per_client', 'bundles'),
                               ('warmup_per_client', 'warmup'), ('fact_rows', 'rows')):
        require(measurement[recorded] == metadata[argument], f'workload mismatch: {recorded}')
    count = metadata['clients'] * metadata['bundles']
    latency = measurement['latency']
    require(set(latency) == set(SQL) | {'bundle'}, 'query classes differ')
    for kind, entry in latency.items():
        require(entry['count'] == count * PER_BUNDLE.get(kind, 1), 'incorrect statement count')
        values = [entry[key] for key in ('p50_ms', 'p95_ms', 'p99_ms', 'max_ms')]
        require(all(math.isfinite(value) and value > 0 for value in values) and values == sorted(values), 'invalid latency quantiles')
    warmup = measurement['warmup']
    require(len(warmup) == metadata['clients'] and sorted(item['client'] for item in warmup) == list(range(metadata['clients']))
            and all(item['passed'] is True for item in warmup), 'missing or failed warmup')
    expected_types = {kind: [23, 20] if kind in ('read', 'update') else [20, 20] if kind == 'range' else [23, 20, 20] for kind in SQL}
    require(all(item['types'] == expected_types for item in warmup), 'result types differ')
    before, after = measurement['resource_before'], measurement['resource_after']
    for counter, field in (('cpu', 'cpu_delta'), ('events', 'memory_event_delta')):
        keys = before[counter].keys() | after[counter].keys()
        difference = {key: after[counter].get(key, 0) - before[counter].get(key, 0) for key in keys}
        require(all(value >= 0 for value in difference.values()) and difference == measurement[field], 'counter reset or inconsistent delta')
    for resource in (before, after, report['resources_after_shutdown']):
        require(resource['memory_swap_current'] == 0 and not any(resource['events'].get(key, 0)
                for key in ('oom', 'oom_kill', 'oom_group_kill')), 'OOM or swap use')
    cpu = measurement['cpu_delta']['usage_usec']
    require(cpu > 0 and math.isclose(cpu / count, measurement['server_cpu_us_per_bundle'], rel_tol=1e-12)
            and math.isclose(cpu / 1e6, measurement['server_cpu_seconds'], rel_tol=1e-12), 'CPU metric inconsistent')
    elapsed = measurement['elapsed_seconds']
    require(elapsed > 0 and math.isfinite(elapsed) and math.isclose(count / elapsed, measurement['bundles_per_second'], rel_tol=1e-12), 'throughput inconsistent')
    samples = measurement['sampled_resources']
    require(samples['samples'] >= 1 and samples['incomplete_process_samples'] == 0, 'incomplete memory samples')
    require(all(samples[field] > 0 for field in ('sampled_memory_current_peak_bytes', 'sampled_summed_rss_peak_bytes',
                                               'sampled_summed_pss_peak_bytes')), 'missing memory measurements')
    expected = expected_updates(metadata['clients'], metadata['bundles'] + metadata['warmup'])
    state = {'changed_accounts': len(expected), 'committed_updates': metadata['clients'] * (metadata['bundles'] + metadata['warmup']) * 4,
             'values_sha256': hashlib.sha256(json.dumps(expected).encode()).hexdigest()}
    require(report['state_before_restart'] == state and report['state_after_restart'] == state, 'committed data or restart verification differs')
    require(set(report['plans']) == {'aggregate', 'range', 'join'}, 'missing diagnostic plans')
    return measurement


def analyze(root, source_dir):
    metadata = json.loads((root / 'metadata.json').read_text())
    summary = json.loads((root / 'summary.json').read_text())
    require(summary['complete'] is True and summary['passed'] is True, 'incomplete or failed campaign')
    binaries = metadata.get('binaries', {})
    require(set(binaries) == {'baseline', 'candidate'}, 'missing binary identities')
    require(all(re.fullmatch(r'[0-9a-f]{64}', entry.get('sha256', '')) for entry in binaries.values()), 'invalid binary digest')
    require(binaries['baseline']['sha256'] != binaries['candidate']['sha256'], 'baseline and candidate are the same binary')
    require(metadata['statements_per_bundle'] == PER_BUNDLE, 'query mix differs')
    require(metadata['repeats'] >= 5, 'at least five paired repetitions required')
    require(metadata['memory_mib'] == [1024, 2048, 4096, 8192], 'complete 1/2/4/8 GiB matrix required')
    require(not set(metadata['server_cpus']) & set(metadata['client_cpus']), 'client and server CPUs overlap')
    expected_sources = {'resource_workload.py', 'resource_scope.py', 'fixed_workload.py', 'cluster.py'}
    require(set(metadata['source_lf_sha256']) == expected_sources, 'harness source set differs')
    for name, digest in metadata['source_lf_sha256'].items():
        actual = hashlib.sha256((source_dir / name).read_bytes().replace(b'\r\n', b'\n')).hexdigest()
        require(actual == digest, f'harness source differs: {name}')
    expected = {(label, memory, repetition) for label in ('baseline', 'candidate')
                for memory in metadata['memory_mib'] for repetition in range(1, metadata['repeats'] + 1)}
    require(len(summary['trials']) == len(expected), 'missing or duplicated trials')
    measurements = {}
    for item in summary['trials']:
        identity = item['label'], item['memory_mib'], item['repetition']
        require(identity in expected and identity not in measurements, 'unknown or duplicate trial')
        filename = f'{identity[0]}-{identity[1]}-{identity[2]}.json'
        require(item['file'] == filename and item['passed'] is True and not item.get('error'), 'invalid trial record')
        report = json.loads((root / filename).read_text())
        measurements[identity] = validate_trial(report, metadata, identity)
        require(item['cpu_us_per_bundle'] == measurements[identity]['server_cpu_us_per_bundle']
                and item['bundles_per_second'] == measurements[identity]['bundles_per_second'], 'summary differs from raw report')
    results = []
    for memory in metadata['memory_mib']:
        baseline, candidate = [[measurements[label, memory, repetition] for repetition in range(1, metadata['repeats'] + 1)]
                               for label in ('baseline', 'candidate')]

        def compare(read):
            return comparison([read(item) for item in baseline], [read(item) for item in candidate])

        item = {'memory_mib': memory, 'cpu_us_per_bundle': compare(lambda m: m['server_cpu_us_per_bundle']),
                'bundles_per_second': compare(lambda m: m['bundles_per_second']),
                'elapsed_seconds': compare(lambda m: m['elapsed_seconds']), 'latency_ms': {}, 'sampled_memory_mib': {}}
        for kind in (*SQL, 'bundle'):
            item['latency_ms'][kind] = {f'p{p}': compare(lambda m: m['latency'][kind][f'p{p}_ms']) for p in (50, 95, 99)}
        for metric in ('memory_current', 'summed_rss', 'summed_pss'):
            item['sampled_memory_mib'][metric] = compare(lambda m: m['sampled_resources'][f'sampled_{metric}_peak_bytes'] / 1024 ** 2)
        item['median_throughput_within_five_percent'] = item['bundles_per_second']['median_change_pct'] >= -5
        item['median_p99_within_five_percent'] = all(query['p99']['median_change_pct'] <= 5 for query in item['latency_ms'].values())
        item['all_pairs_lower_cpu'] = item['cpu_us_per_bundle']['paired_change_pct']['max'] < 0
        item['memory_limit_events'] = {label: [measurements[label, memory, repetition]['memory_event_delta'].get('max', 0)
                                              for repetition in range(1, metadata['repeats'] + 1)] for label in ('baseline', 'candidate')}
        item['cpu_throttled_usec'] = {label: [measurements[label, memory, repetition]['cpu_delta'].get('throttled_usec', 0)
                                             for repetition in range(1, metadata['repeats'] + 1)] for label in ('baseline', 'candidate')}
        results.append(item)
    return {'evidence_valid': True, 'production_qualified': False, 'trials': len(measurements),
            'measured_statements': len(measurements) * metadata['clients'] * metadata['bundles'] * sum(PER_BUNDLE.values()),
            'metadata': metadata, 'limits': results,
            'interpretation': 'Observed five-run distributions on this host, not a confidence interval or production acceptance. Inspect paired variation and query-specific tails; memory caps do not establish overload safety.'}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--input-dir', type=Path, required=True)
    parser.add_argument('--source-dir', type=Path, default=Path(__file__).parent)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    result = analyze(args.input_dir, args.source_dir)
    with args.output.open('x') as stream:
        json.dump(result, stream, indent=2)
        stream.write('\n')
    for item in result['limits']:
        print(json.dumps({'memory_mib': item['memory_mib'], 'cpu_change_pct': item['cpu_us_per_bundle']['median_change_pct'],
                          'throughput_change_pct': item['bundles_per_second']['median_change_pct'],
                          'median_p99_within_five_percent': item['median_p99_within_five_percent']}))


if __name__ == '__main__':
    main()

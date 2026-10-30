import copy
import hashlib
import json
from pathlib import Path
import tempfile
import unittest

from compare_resource import analyze, comparison
from fixed_workload import PER_BUNDLE, SQL, expected_updates


class ResourceEvidenceTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.sources = self.root / 'sources'
        self.sources.mkdir()
        hashes = {}
        for name in ('resource_workload.py', 'resource_scope.py', 'fixed_workload.py', 'cluster.py'):
            (self.sources / name).write_text('# synthetic unit fixture\n')
            hashes[name] = hashlib.sha256((self.sources / name).read_bytes().replace(b'\r\n', b'\n')).hexdigest()
        self.metadata = {'statements_per_bundle': PER_BUNDLE, 'repeats': 5, 'clients': 1, 'bundles': 2,
                         'warmup': 1, 'rows': 4096, 'memory_mib': [1024, 2048, 4096, 8192],
                         'server_cpus': [0, 2, 4, 6], 'client_cpus': [8, 10], 'source_lf_sha256': hashes}
        self.metadata['binaries'] = {'baseline': {'sha256': '0' * 64}, 'candidate': {'sha256': '1' * 64}}
        expected = expected_updates(1, 3)
        state = {'changed_accounts': len(expected), 'committed_updates': 12,
                 'values_sha256': hashlib.sha256(json.dumps(expected).encode()).hexdigest()}
        before = {'cpu': {'usage_usec': 10}, 'events': {'oom': 0, 'oom_kill': 0, 'oom_group_kill': 0, 'max': 0}, 'memory_swap_current': 0}
        after = copy.deepcopy(before)
        after['cpu']['usage_usec'] = 1010
        measurement = {'passed': True, 'clients': 1, 'bundles_per_client': 2, 'warmup_per_client': 1,
                       'fact_rows': 4096, 'cpu_delta': {'usage_usec': 1000}, 'memory_event_delta': before['events'],
                       'resource_before': before, 'resource_after': after, 'server_cpu_us_per_bundle': 500,
                       'server_cpu_seconds': 0.001, 'elapsed_seconds': 0.2, 'bundles_per_second': 10,
                       'sampled_resources': {'samples': 4, 'incomplete_process_samples': 0,
                           'sampled_memory_current_peak_bytes': 128 * 1024 ** 2,
                           'sampled_summed_rss_peak_bytes': 140 * 1024 ** 2,
                           'sampled_summed_pss_peak_bytes': 120 * 1024 ** 2},
                       'latency': {kind: {'count': 2 * PER_BUNDLE.get(kind, 1), 'p50_ms': 0.1, 'p95_ms': 0.2,
                                          'p99_ms': 0.3, 'max_ms': 0.4} for kind in (*SQL, 'bundle')},
                       'warmup': [{'client': 0, 'passed': True, 'types': {
                           kind: [23, 20] if kind in ('read', 'update') else [20, 20] if kind == 'range' else [23, 20, 20] for kind in SQL}}]}
        self.summary = {'passed': True, 'complete': True, 'trials': []}
        for label in ('baseline', 'candidate'):
            for memory in self.metadata['memory_mib']:
                for repetition in range(1, 6):
                    filename = f'{label}-{memory}-{repetition}.json'
                    report = {'passed': True, 'label': label, 'memory_mib': memory, 'repetition': repetition,
                              'remaining_server_pids': [], 'limits': {'cpu.max': '400000 100000',
                                  'memory.max': str(memory * 1024 ** 2), 'memory.swap.max': '0', 'pids.max': '1024'},
                              'settings': [['fsync', 'on', None], ['synchronous_commit', 'on', None],
                                           ['max_active_queries', '4', None], ['work_mem', '16384', 'kB']],
                              'measurement': measurement, 'resources_after_shutdown': after,
                              'state_before_restart': state, 'state_after_restart': state,
                              'plans': {kind: [] for kind in ('aggregate', 'range', 'join')}}
                    self.write(filename, report)
                    self.summary['trials'].append({'file': filename, 'passed': True, 'label': label, 'memory_mib': memory,
                                                  'repetition': repetition, 'cpu_us_per_bundle': 500, 'bundles_per_second': 10})
        self.write('metadata.json', self.metadata)
        self.write('summary.json', self.summary)

    def write(self, name, value):
        (self.root / name).write_text(json.dumps(value))

    def test_complete_evidence_reports_all_work_without_qualifying_production(self):
        result = analyze(self.root, self.sources)
        self.assertTrue(result['evidence_valid'])
        self.assertFalse(result['production_qualified'])
        self.assertEqual(result['trials'], 40)
        self.assertEqual(result['measured_statements'], 40 * 2 * 19)
        self.assertEqual(result['limits'][0]['cpu_us_per_bundle']['paired_change_pct']['values'], [0] * 5)

    def test_incomplete_missing_and_duplicate_runs_fail_closed(self):
        for mutate in (lambda s: s.update(complete=False), lambda s: s['trials'].pop(),
                       lambda s: s['trials'].__setitem__(0, s['trials'][1])):
            with self.subTest(mutate=mutate):
                value = copy.deepcopy(self.summary)
                mutate(value)
                self.write('summary.json', value)
                with self.assertRaises(ValueError):
                    analyze(self.root, self.sources)

    def test_wrong_workload_source_or_matrix_fails_closed(self):
        for mutate in (lambda m: m['statements_per_bundle'].update(read=11), lambda m: m.update(repeats=1),
                       lambda m: m.update(memory_mib=[1024]), lambda m: m.update(client_cpus=[0]),
                       lambda m: m.update(binaries={}),
                       lambda m: m['binaries']['candidate'].update(sha256='0' * 64),
                       lambda m: m['source_lf_sha256'].update({'../../escape': '0' * 64}),
                       lambda m: m['source_lf_sha256'].update({'cluster.py': '0' * 64})):
            with self.subTest(mutate=mutate):
                value = copy.deepcopy(self.metadata)
                mutate(value)
                self.write('metadata.json', value)
                with self.assertRaises(ValueError):
                    analyze(self.root, self.sources)

    def test_bad_types_counts_resources_and_durability_fail_closed(self):
        name = 'baseline-1024-1.json'
        original = json.loads((self.root / name).read_text())
        mutations = [lambda r: r.update(remaining_server_pids=['123']),
                     lambda r: r['limits'].update({'memory.max': 'max'}),
                     lambda r: r['settings'][0].__setitem__(1, 'off'),
                     lambda r: r['measurement']['latency']['read'].update(count=23),
                     lambda r: r['measurement']['warmup'][0]['types'].update(read=[23, 23]),
                     lambda r: r['measurement']['cpu_delta'].update(usage_usec=-1),
                     lambda r: r['measurement'].update(server_cpu_us_per_bundle=499),
                     lambda r: r['measurement'].update(bundles_per_second=11),
                     lambda r: r['measurement']['sampled_resources'].update(incomplete_process_samples=1),
                     lambda r: r['resources_after_shutdown']['events'].update(oom_kill=1),
                     lambda r: r['resources_after_shutdown'].update(memory_swap_current=4096),
                     lambda r: r['state_after_restart'].update(values_sha256='0' * 64)]
        for mutate in mutations:
            with self.subTest(mutate=mutate):
                value = copy.deepcopy(original)
                mutate(value)
                self.write(name, value)
                with self.assertRaises(ValueError):
                    analyze(self.root, self.sources)

    def test_summary_cannot_replace_raw_measurements(self):
        self.summary['trials'][0]['cpu_us_per_bundle'] = 1
        self.write('summary.json', self.summary)
        with self.assertRaisesRegex(ValueError, 'summary differs'):
            analyze(self.root, self.sources)

    def test_query_specific_tail_regression_is_reported(self):
        for repetition in range(1, 6):
            name = f'candidate-1024-{repetition}.json'
            value = json.loads((self.root / name).read_text())
            value['measurement']['latency']['read']['p99_ms'] = 0.33
            self.write(name, value)
        result = analyze(self.root, self.sources)
        self.assertFalse(result['limits'][0]['median_p99_within_five_percent'])
        self.assertAlmostEqual(result['limits'][0]['latency_ms']['read']['p99']['median_change_pct'], 10)

    def test_paired_changes_preserve_variation_hidden_by_medians(self):
        result = comparison([10, 20, 30, 40, 50], [20, 20, 30, 40, 50])
        self.assertEqual(result['median_change_pct'], 0)
        self.assertEqual(result['paired_change_pct']['values'], [100, 0, 0, 0, 0])
        for invalid in (0, -1, float('nan'), float('inf')):
            with self.assertRaises(ValueError):
                comparison([1], [invalid])


if __name__ == '__main__':
    unittest.main()

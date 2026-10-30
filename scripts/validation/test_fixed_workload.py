from collections import Counter
from pathlib import Path
import unittest

from fixed_workload import PER_BUNDLE, expected_updates, grouped, latency_summary, progression_sum
from resource_scope import delta, private_unit


class FixedWorkloadTests(unittest.TestCase):
    def test_independent_cycle_oracle_matches_enumeration(self):
        for first in (1, 995, 4096, 99999):
            for count in (0, 1, 124, 125, 126, 1000, 1031, 20001):
                for stride in (1, 8, 128):
                    self.assertEqual(progression_sum(first, count, stride), sum((17 * (first + i * stride)) % 1000 for i in range(count)))
        for lo, hi in ((1, 4096), (100, 8199), (7, 100007)):
            for groups in (8, 128):
                counts, sums = Counter(), Counter()
                for i in range(lo, hi + 1):
                    counts[i % groups] += 1
                    sums[i % groups] += (17 * i) % 1000
                self.assertEqual(grouped(lo, hi, groups), [(key, counts[key], sums[key]) for key in sorted(counts)])

    def test_fixed_mix_and_disjoint_acknowledged_update_ownership(self):
        self.assertEqual(sum(PER_BUNDLE.values()), 19)
        self.assertEqual(PER_BUNDLE['read'], 12)
        self.assertEqual(PER_BUNDLE['update'], 4)
        expected = expected_updates(6, 450)
        self.assertEqual(sum(count for _, count in expected), 6 * 450 * 4)
        for client in range(6):
            self.assertEqual(sum(count for key, count in expected if client * 20000 < key <= (client + 1) * 20000), 450 * 4)

    def test_quantiles_count_every_sample_and_counter_reset_fails(self):
        result = latency_summary(range(1, 101))
        self.assertEqual(result, {'count': 100, 'p50_ms': 50 / 1e6, 'p95_ms': 95 / 1e6, 'p99_ms': 99 / 1e6, 'max_ms': 100 / 1e6})
        with self.assertRaises(ValueError):
            latency_summary([])
        self.assertEqual(delta({'usage_usec': 100}, {'usage_usec': 150, 'nr_throttled': 2}), {'usage_usec': 50, 'nr_throttled': 2})
        with self.assertRaises(ValueError):
            delta({'usage_usec': 100}, {'usage_usec': 99})

    def test_only_private_service_units_are_eligible(self):
        self.assertTrue(private_unit(Path('/sys/fs/cgroup/system.slice/rustsql-qualification-example.service')))
        for path in ('/sys/fs/cgroup', '/sys/fs/cgroup/system.slice', '/sys/fs/cgroup/system.slice/other.service',
                     '/tmp/system.slice/rustsql-qualification-example.service', '/sys/fs/cgroup/system.slice/rustsql-qualification-example.scope'):
            self.assertFalse(private_unit(Path(path)))


if __name__ == '__main__':
    unittest.main()

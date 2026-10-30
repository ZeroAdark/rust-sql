import copy
import unittest

from regression_capture import SMOKE_CHECKS, validate_run


class RegressionCaptureTests(unittest.TestCase):
    def report(self, code=1):
        report = {'cluster': '/owned/cluster', 'checks': SMOKE_CHECKS.copy(), 'version': 'PostgreSQL 18.6 (pgrust)',
                  'passed': code == 0, 'regression': {'returncode': code, 'profile': 'postgresql-raw'}}
        if code:
            report['error'] = 'PostgreSQL suites failed: see logs in /owned/cluster'
        return report

    def test_raw_verdict_preserved_for_completed_capture(self):
        for code in (0, 1):
            report = self.report(code)
            before = copy.deepcopy(report)
            validate_run(report)
            self.assertEqual(report, before)
            report['capture_complete'] = True
            validate_run(report)

    def test_missing_checks_abnormal_runners_and_other_failures_block(self):
        changes = [
            ('checks', SMOKE_CHECKS[:-1]), ('checks', SMOKE_CHECKS + [SMOKE_CHECKS[0]]),
            ('version', 'PostgreSQL 18.6'), ('passed', True), ('passed', None),
            ('error', 'server shutdown failed'), ('error', None),
            ('regression', {}), ('regression', {'returncode': 2, 'profile': 'postgresql-raw'}),
            ('regression', {'returncode': -9, 'profile': 'postgresql-raw'}),
            ('regression', {'returncode': True, 'profile': 'postgresql-raw'}),
            ('regression', {'returncode': 1, 'profile': 'other'}),
            ('isolation', {'returncode': 1}), ('isolation', {'returncode': False}),
            ('mixed', {'errors': ['failed query']}),
            ('capture_complete', False),
        ]
        for key, value in changes:
            report = self.report()
            report[key] = value
            with self.subTest(key=key, value=value), self.assertRaises(ValueError):
                validate_run(report)
        report = self.report(0)
        report['error'] = 'connection failed'
        with self.assertRaises(ValueError):
            validate_run(report)


if __name__ == '__main__':
    unittest.main()

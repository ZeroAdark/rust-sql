"""Validate successful capture of raw expectations without hiding runner errors."""

SMOKE_CHECKS = [
    'durability_settings', 'heap_aggregate', 'rollback', 'savepoint',
    'plan_invalidation', 'columnar_aggregate', 'columnar_rollback',
    'columnar_writes', 'concurrent_committed_updates', 'clean_restart',
    'acknowledged_commit_survives_kill', 'columnar_survives_restart',
]


def validate_run(report):
    if report.get('checks') != SMOKE_CHECKS or 'pgrust' not in report.get('version', ''):
        raise ValueError('candidate smoke checks incomplete or wrong server')
    regression = report.get('regression', {})
    code = regression.get('returncode')
    if type(code) is not int or code not in (0, 1) or regression.get('profile') != 'postgresql-raw':
        raise ValueError('raw regression runner failed or missing')
    if 'isolation' in report:
        isolation_code = report['isolation'].get('returncode')
        if type(isolation_code) is not int or isolation_code != 0:
            raise ValueError('isolation failed')
    if 'mixed' in report and report['mixed'].get('errors') != []:
        raise ValueError('mixed workload failed')
    if code == 0:
        if report.get('passed') is not True or 'error' in report:
            raise ValueError('candidate failed outside regression comparison')
    else:
        message = f"PostgreSQL suites failed: see logs in {report.get('cluster')}"
        if not report.get('cluster') or report.get('passed') is not False or report.get('error') != message:
            raise ValueError('unexpected candidate failure')
    if report.get('capture_complete', True) is not True:
        raise ValueError('raw capture did not complete')

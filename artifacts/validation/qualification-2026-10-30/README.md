# Reviewed regression and TLS evidence

Runtime source: `faf62fc4a544b2d033d812b9728b54ecfcf390ad` (same runtime files as
`41b6b672d8a80bc2b7f7f737f583167c1562e516`). No server implementation changed in
this validation checkpoint. Binary SHA-256:
`a5a76016a579d93da8e6cb73c95a419b7130ceba91c140bde6cdb017e2b0717b`.
The executable's older build label is explained by the existing
[source/binary manifest](../conformance-2026-10-29/README.md).

- `engine-first.json` and `engine-repeat.json`: 215/231 accepted, 16 unresolved,
  using nine reviewed profiles containing 35 exact direct EXPLAIN tables.
  Both reports intentionally say `passed: false`.
- `repeat-suites.json`: twelve smoke checks pass; raw PostgreSQL regression is
  203/231, so this report also correctly says `passed: false`.
- `auth-tls.json`: all 21 TLS/SCRAM, certificate, reload and key-permission checks
  pass. Client: psycopg 3.1.17 with libpq 16.0.15, on Linux x86-64.
- `comparator-tests.log`: 48 tests pass, including fail-closed profile/audit cases.
- `raw-evidence.tar.xz`: the new full raw regression output and logs, plus both
  complete engine audits with SQL-order proofs and unresolved diffs. Hashes and
  input identities are recorded in `manifest.json`.

The native control still matches all 231 pinned PostgreSQL expectations. Its raw
output and the first candidate run are already retained in the preceding
[conformance archive](../conformance-2026-10-29/README.md). Optional features can
select different whole-file upstream alternatives; the auditor never combines
parts of alternatives. All source hashes and original plan hashes must match
before applying a reviewed profile. Actual SQL values, errors and plan-helper
results receive no new normalization.

Reproduce the new checks after the documented raw/native suite commands, retaining
their JSON reports:

```sh
python3 -m unittest discover -s scripts/validation -p 'test_*.py'
python3 scripts/validation/engine_regression_audit.py \
  --fixtures crates/postgres-18.6-reference/src/test/regress \
  --reference-report artifacts/native-regression.json \
  --candidate-report artifacts/postgres-raw.json --output artifacts/engine-contract
ulimit -s 65520
python3 scripts/validation/auth_tls_probe.py --candidate target/release \
  --work-dir /tmp/rustsql-security-checks --output artifacts/auth-tls.json
```

The security probe binds only 127.0.0.1:55440 and refuses to use an occupied port.
TCP never starts with trust authentication. Private Unix sockets provide setup
access; generated keys remain mode 0600 inside a mode-0700 cluster directory
except for the deliberate permission-rejection test. No private key, password,
data directory or certificate-signing material is committed or archived here.

This checkpoint provides no new CPU-saving claim and does not qualify production
use. Remaining compatibility, client, recovery, overload, fuzzing and soak gates
are tracked in [production readiness](../../../docs/production-readiness.md).

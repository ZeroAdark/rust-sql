# Retained allocator-residency runtime: local qualification

The retained `02cfbca6` binary passes a fresh reviewed 231/231 SQL contract and 510/510 isolation cases. Its raw PostgreSQL result remains 203 passes and 28 documented differences; no PostgreSQL expectations or reviewed policies were broadened. The earlier frozen native PostgreSQL control is reused, with its exact report and SQL transcripts bound by hashes. The captured raw failure is intentional evidence, not a hidden pass.

Fresh probes also pass: 12 smoke checks, 87 differential checks, 52 admission checks, 12 connection checks, 12 idle-session checks, 21 authentication/TLS checks, ten whole-process crash cycles plus logical restore, and 220 independent window-oracle comparisons per corrected engine. The window control is separately patched PostgreSQL; the ordinary performance reference remains native 18.6.

This evidence applies only to SHA-256 `02cfbca6869bb352c8081b1fb275cdd4317b33a72b8feb05c563408e4c48d454`, corresponding to the accounting correction at `d0c1cd2`. The test runner's parent is the later documentation-only `d1a5b20`. It does not qualify the rejected standby prototype or the subsequent configuration-name-index experiment. GitHub Actions was disabled; all execution was local WSL Linux x86-64.

The archive retains commands, source, reviewed policies, all SQL/native/result transcripts, all isolation inputs/results, server logs and probe reports. Ephemeral TLS private keys, passwords and database files are not included. The binary's release build and earlier counter/protocol/spill evidence remain in the allocator-residency archive.

Run `python3 replay.py` to verify archive integrity, transcript-to-audit hashes, complete coverage and recorded probe results. It neither extracts nor executes archived code and does not rerun SQL. A deliberately changed SQL transcript was rejected. This closes the retained binary's local compatibility rerun gap; a host with anonymous-mapping-name support, valid capacity performance evidence, the four PostgreSQL memory/latency targets, sustained pressure/recovery and representative-server qualification remain open. Production status remains NOT QUALIFIED.

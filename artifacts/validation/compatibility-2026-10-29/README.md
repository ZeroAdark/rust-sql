# Compatibility and hardening evidence

Captured on Linux x86-64 under Ubuntu 24.04 WSL, Rust 1.96.0, against the tracked native PostgreSQL 18.6 reference. These are development qualification results, not a production certification. `manifest.json` records the exact binary hashes and changed production inputs; the version banner retains the base commit because the build used its working-tree patch.

| Check | Result |
| --- | --- |
| Locked complete server release build | Pass |
| Compression, backup, columnar codec, WAL reader/writer, Parquet unit tests | 221 passed, 5 existing ignored |
| Existing executor admission/instrumentation integration tests | 4 passed |
| Wasm backend integration suite after min/max oracle correction | 51 passed |
| Native PostgreSQL differential driver | 88 passed |
| Existing upstream engine isolation profile plus extra schedule | 510/510 passed |
| Strict unmodified PostgreSQL regression output | 204/231 passed, 27 failed; raw diffs retained |
| Whole-process kill, logical restore, ACL and timeout probe | Ten crash cycles plus role/restore checks passed |
| Same recovery probe, 1 GiB cgroup / no swap / four-CPU quota | Pass, zero OOM events |
| cargo-audit 0.22.2 | Zero vulnerability matches, zero warnings; database revision in JSON |

`differential-before.json` contains the two reproduced CTAS instrumentation failures. `differential-after.json` additionally includes the smallint-resolution control (both servers return the same ambiguity error). The extra control is not claimed to be newly supported SQL. `wasm-tests-before.log.gz` preserves the signed-zero oracle failure; its corrected test and justification are in the code and investigation document. No executable was changed to satisfy that test.

`suites.json` intentionally has `passed: false`: strict PostgreSQL expectations still fail even though the engine-profile isolation portion passes. `regression.diffs.gz` is the complete raw diff. The broad suite's twelve smoke checks are separate from the stronger whole-process crash probe. The native isolation control runs with PostgreSQL backend workers disabled and compares the existing upstream overlay byte for byte.

Reproduction commands are in [production-readiness.md](../../../docs/production-readiness.md) and the manual Linux CI job. For the constrained probe, run the following from the repository as a user authorized to create a transient system service, substituting the non-root validation user and absolute paths:

```sh
sudo systemd-run --wait --pipe --collect \
  -p User=rustsql -p MemoryMax=1G -p MemorySwapMax=0 -p CPUQuota=400% \
  -p LimitSTACK=67108864 -p RuntimeMaxSec=180 --working-directory=/home/rustsql/pgrust \
  /usr/bin/python3 scripts/validation/recovery_probe.py \
  --candidate /home/rustsql/qualified-candidate/bin --reference-clients /home/rustsql/postgres18/bin \
  --work-dir /home/rustsql/compat --output /home/rustsql/qualification-recovery-1g.json --cycles 10
```

When staging binaries outside `target/release`, copy its `share/` directory beside `initdb` as well. The cgroup report measures the whole disposable probe service, including clients and clusters; it is not a per-query memory measurement. The 968-row-per-table recovery dataset does not establish overload behavior. No representative CPU improvement is inferred from these checks.

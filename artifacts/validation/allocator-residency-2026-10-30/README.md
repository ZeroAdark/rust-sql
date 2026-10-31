# Allocator residency availability

The corrected diagnostic omits incomplete allocator totals on hosts without
anonymous VMA names. The retained runtime reports zero mimalloc residency on
this WSL2 kernel despite nonzero process/cgroup memory; Hearth reports only its
independently measured pool. The complete four-case run reproduces both old
readings and verifies their omission with the fix, before/after mixed work and
after restart. Process/cgroup measurements remain present.

The candidate is built from `5ef8372` plus the archived source patch. Its server
SHA-256 is `02cfbca6869bb352c8081b1fb275cdd4317b33a72b8feb05c563408e4c48d454`.
The retained server is `8e5bfc2b093055a5d2257dc817037a2b2f2906faf7c1b83e431f472a2f0513f0`.
All checks ran locally; GitHub Actions remain disabled.

- The locked release build and all 61 memory-runtime unit tests pass. Bypassing
  the availability guard makes the new test fail at the false-zero assertion;
  restoring the source makes it pass. The reclaim calculation's body is unchanged.
- Four complete live cases cover both allocators on both binaries, 18,240 checked
  statements including warmup, and 3,840 committed updates verified after restart.
  All eight shutdowns are normal; no server processes remain.
- All eight Bind/portal/protocol groups match PostgreSQL 18.6.
- The 500,000-row, 1 GiB spill probe passes all six result/hash cases per engine,
  physical spill witnesses, temp-file limits, cancellation and cleanup. Both
  engines shut down normally, with no OOM kills or swap use.

The first live attempt is retained as incomplete: both old-runtime controls
passed, then candidate initialization failed because the frozen binary directory
lacked `share` files. The packaging repair copies and hashes 876 files identical
to the retained installation. The complete repeat uses the same server binary.

This verifies diagnostic behavior on a kernel without VMA names. A supported
kernel's reporting path, a new full compatibility run, and performance
qualification of this exact binary remain open. These diagnostic runs are not
performance acceptance trials. The four PostgreSQL memory/latency targets and
the known sustained million-row sort boundary remain open.

Run `python3 replay.py` to verify every archived member's hash and recompute the
reported checks from saved evidence. No archive member is extracted or executed.

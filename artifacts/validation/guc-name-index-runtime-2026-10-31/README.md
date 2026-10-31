# Shared configuration-name index: runtime evidence

The `57baa2ef` binary passes 231/231 reviewed SQL cases and 510 isolation cases. Raw PostgreSQL output remains 203 passes and 28 documented differences. The existing compatibility policy is unchanged; the raw failure remains recorded.

The change shares the immutable built-in name-to-slot index independently of thread retention. Session values, hooks and custom settings remain private. Name lookup remains expected O(1); registry initialization still initializes each setting. The source patch and exact build inputs are included. All 101 GUC tests pass, and restoring the old default makes the new default-sharing test fail as intended.

Fresh runtime probes pass: 12 smoke, 12 idle, 12 connection, 52 admission, 87 differential, 21 TLS/authentication, ten crash cycles plus logical restore, and 220 window-oracle checks per corrected engine. The window reference is separately patched PostgreSQL; performance uses native PostgreSQL 18.6.

The archive binds the SQL transcripts, policies, source and binary hashes. The prior native SQL control is reused and preserved. It excludes ephemeral TLS keys, credentials and database files. Run `python3 replay.py` to verify recorded evidence without extracting or executing archived contents; it does not rerun the server. A changed SQL transcript is rejected, and Python optimized mode is refused.

Medium/capacity and startup comparisons are in the adjacent [performance evidence](../guc-name-index-performance-2026-10-31/README.md). This supports retaining the change locally, not a production release. Memory parity, point-read/update latency parity, extended operational qualification and a representative Linux server remain open. GitHub Actions stayed disabled.

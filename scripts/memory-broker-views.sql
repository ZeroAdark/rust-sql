-- Memory step 6a (lane membroker): the memory broker's views over its pgrust-native builtins (reserved-range internal
-- names 9070 / 9071, gravity_hosts membroker/view.rs); G1-A adds the memory ledger's accounts (9072) and the running
-- statements' Gravity region bytes by kind (9073). Created on demand by a superuser: the shipped catalog carries no
-- delta, and stock PostgreSQL errors identically on the unknown internal names. Re-runnable: the budget row's columns grew
-- (memory step 5b's reclaimer), the ledger's too (G1-B: pool_reserved_*, the kinds' mapped_*), the consumer rows' too (the
-- caches' value counters: hits, misses, miss_bytes, miss_cost_*, re_miss*; then their tail-hit and ghost counters under
-- PGRUST_MEMPOLICY=broker_value=1: evicted_bytes, ghost_hits, tail_hits, tail_bytes, refault_log2), and a function's OUT
-- columns change only through a drop.
DROP VIEW IF EXISTS pgrust_memory_broker;
DROP FUNCTION IF EXISTS pgrust_memory_broker_stats();
DROP VIEW IF EXISTS pgrust_memory_budget;
DROP FUNCTION IF EXISTS pgrust_memory_budget();
DROP VIEW IF EXISTS pgrust_memory_accounts;
DROP FUNCTION IF EXISTS pgrust_memory_accounts();
DROP VIEW IF EXISTS pgrust_memory_account_kinds;
DROP FUNCTION IF EXISTS pgrust_memory_account_kinds();
DROP VIEW IF EXISTS pgrust_memory_owner_end;
DROP FUNCTION IF EXISTS pgrust_memory_owner_end();
DROP VIEW IF EXISTS pgrust_memory_owners;
DROP FUNCTION IF EXISTS pgrust_memory_owners();
DROP VIEW IF EXISTS pgrust_memory_reconcile;
DROP FUNCTION IF EXISTS pgrust_memory_reconcile();
DROP VIEW IF EXISTS pgrust_memory_ladder;
DROP FUNCTION IF EXISTS pgrust_memory_ladder_stats();
DROP VIEW IF EXISTS pgrust_memory_ladder_episodes;
DROP FUNCTION IF EXISTS pgrust_memory_ladder_episodes();
DROP VIEW IF EXISTS pgrust_memory_revoke;
DROP FUNCTION IF EXISTS pgrust_memory_revoke_stats();
DROP VIEW IF EXISTS pgrust_memory_cache_curves;
DROP FUNCTION IF EXISTS pgrust_memory_cache_curves();
DROP VIEW IF EXISTS pgrust_memory_settings;
DROP FUNCTION IF EXISTS pgrust_memory_settings();
CREATE OR REPLACE FUNCTION pgrust_memory_broker_stats(
    OUT consumer text, OUT current_bytes int8, OUT cap_bytes int8, OUT floor_bytes int8, OUT ceiling_bytes int8,
    OUT desired_bytes int8, OUT target_bytes int8, OUT elastic_bytes int8, OUT reused_60s int8, OUT reused_300s int8,
    OUT reused_900s int8, OUT refill_class text, OUT advised_move_bytes int8, OUT last_move_bytes int8,
    OUT last_move_age_s int8, OUT reversals int8, OUT frozen boolean, OUT hold text, OUT targeted boolean,
    OUT hits int8, OUT misses int8, OUT miss_bytes int8, OUT miss_cost_ns int8, OUT miss_cost_samples int8,
    OUT re_misses int8, OUT re_miss_bytes int8, OUT evicted_bytes int8, OUT ghost_hits int8, OUT tail_hits int8,
    OUT tail_bytes int8, OUT refault_log2 int8[], OUT small_buffer_bytes int8, OUT small_buffer_held_bytes int8,
    OUT small_buffer_draws int8, OUT small_buffer_waits int8, OUT small_buffer_refusals int8)
RETURNS SETOF record LANGUAGE internal VOLATILE AS 'pgrust_memory_broker_stats';
CREATE OR REPLACE FUNCTION pgrust_memory_budget(
    OUT mode text, OUT limit_bytes int8, OUT b_auto_bytes int8, OUT b_bytes int8, OUT free_b_bytes int8,
    OUT r_low_bytes int8, OUT r_high_bytes int8, OUT page_cache_floor_bytes int8, OUT bar_bytes int8, OUT fixed_bytes int8,
    OUT unattributed_bytes int8, OUT unattributed_p90_bytes int8, OUT unattributed_counted boolean, OUT work_signal_bytes int8,
    OUT work_p90_300s_bytes int8, OUT running_floor_bytes int8, OUT queued_bytes int8, OUT work_demand_bytes int8,
    OUT fits boolean, OUT registry_removed_parts int8, OUT registry_removed_bytes int8, OUT file_bytes_900s int8,
    OUT interval_s int8, OUT plans int8, OUT plan_us_max int8, OUT panics int8, OUT stopped boolean, OUT age_s int8,
    OUT parts int8, OUT parts_used_900s int8, OUT parts_reused_900s int8, OUT reclaimer boolean,
    OUT reclaim_below_low_ticks int8, OUT reclaim_background_bytes int8, OUT reclaim_inline_runs int8,
    OUT reclaim_inline_bytes int8, OUT reclaim_inline_us_max int8, OUT reclaim_inline_short int8,
    OUT worker_cap_refusals int8, OUT worker_cap_refused_bytes int8)
RETURNS SETOF record LANGUAGE internal VOLATILE AS 'pgrust_memory_budget';
CREATE OR REPLACE VIEW pgrust_memory_broker AS SELECT * FROM pgrust_memory_broker_stats();
CREATE OR REPLACE VIEW pgrust_memory_budget AS SELECT * FROM pgrust_memory_budget();
-- C8 move 3c: pool_reserved_bytes is the grant pool's promise not yet backed by a draw (a drawn byte counts once, on
-- account_bytes); pool_reserved_peak_bytes the claim's most (backed or not).
CREATE OR REPLACE FUNCTION pgrust_memory_accounts(
    OUT account text, OUT pid int4, OUT statement_seq int8, OUT running boolean, OUT statement_bytes int8,
    OUT statement_peak_bytes int8, OUT statement_heap_peak_bytes int8, OUT statement_mapped_peak_bytes int8,
    OUT account_bytes int8, OUT heap_bytes int8, OUT mapped_bytes int8, OUT pool_reserved_bytes int8,
    OUT pool_reserved_peak_bytes int8, OUT threads int4, OUT fold_slack_bytes int8, OUT not_resident_bytes int8,
    OUT not_resident_mapped_bytes int8, OUT statement_charged_bytes int8, OUT statement_charged_peak_bytes int8,
    OUT class text)
RETURNS SETOF record LANGUAGE internal VOLATILE AS 'pgrust_memory_accounts';
CREATE OR REPLACE FUNCTION pgrust_memory_account_kinds(
    OUT pid int4, OUT statement_seq int8, OUT kind text, OUT bytes int8, OUT peak_bytes int8, OUT mapped_bytes int8,
    OUT mapped_peak_bytes int8)
RETURNS SETOF record LANGUAGE internal VOLATILE AS 'pgrust_memory_account_kinds';
CREATE OR REPLACE VIEW pgrust_memory_accounts AS SELECT * FROM pgrust_memory_accounts();
CREATE OR REPLACE VIEW pgrust_memory_account_kinds AS SELECT * FROM pgrust_memory_account_kinds();
-- C8 checker 1 (9074): the owner-end residues (memledger owner_end: totals, counters, what sessions and cache holders own
-- now, the latest residues), and checker 2's reconcile terms (kind 'reconcile': the signed 'unattributed', and each cache
-- owner's account less its own count; report only, the stress kit judges them).
CREATE OR REPLACE FUNCTION pgrust_memory_owner_end(
    OUT kind text, OUT source text, OUT class text, OUT events int8, OUT committed_bytes int8, OUT reserved_bytes int8,
    OUT uses int8, OUT account text, OUT pid int4, OUT seq int8)
RETURNS SETOF record LANGUAGE internal VOLATILE AS 'pgrust_memory_owner_end';
CREATE OR REPLACE VIEW pgrust_memory_owner_end AS SELECT * FROM pgrust_memory_owner_end();
-- LEDGER2 (9075): the owner tree, one row per owner (session > statement / portal > operator > worker_arena, caches, exit
-- and orphan cells): held split by source (contexts named / inferred, charges; held_mapped = door 2's of held), budget (promised),
-- claimed_unused = budget - held (signed; a statement's and a portal's: budget - subtree_held), reserved_unbacked, spilled,
-- the subtree sums, uses, held_not_resident / subtree_not_resident (of held / subtree_held, the residency pass's not-resident
-- bytes: held - held_not_resident is in memory); one 'keepers' row per account with any and one for the process (the
-- context blocks mcx parked for the next context, held; of them parked_growth_bytes / parked_growth_count, D4679: the
-- growth shelf's, NULL on every other row); then one 'unattributed' row per account and one for the process (the
-- account's bytes less its owners' and its keepers, signed).
CREATE OR REPLACE FUNCTION pgrust_memory_owners(
    OUT kind text, OUT owner_id int8, OUT cell int4, OUT parent_cell int4, OUT account text, OUT pid int4, OUT label text,
    OUT ended boolean, OUT held_ctx_named int8, OUT held_ctx_inferred int8, OUT held_charges int8, OUT held_mapped int8,
    OUT held int8, OUT budget int8, OUT claimed_unused int8, OUT reserved_unbacked int8, OUT spilled int8,
    OUT subtree_held int8, OUT subtree_spilled int8, OUT uses int8, OUT held_not_resident int8, OUT subtree_not_resident int8,
    OUT parked_growth_bytes int8, OUT parked_growth_count int8)
RETURNS SETOF record LANGUAGE internal VOLATILE AS 'pgrust_memory_owners';
CREATE OR REPLACE VIEW pgrust_memory_owners AS SELECT * FROM pgrust_memory_owners();
-- LEDGER2 (9076): the process total term by term against the limiting cgroup's memory.current (source 'reading'), the
-- signed gap (reading - terms; the band is the user's), and information rows never summed.
CREATE OR REPLACE FUNCTION pgrust_memory_reconcile(OUT term text, OUT bytes int8, OUT source text, OUT note text)
RETURNS SETOF record LANGUAGE internal VOLATILE AS 'pgrust_memory_reconcile';
CREATE OR REPLACE VIEW pgrust_memory_reconcile AS SELECT * FROM pgrust_memory_reconcile();
-- Lane ladderview (9077-9079): the memory ladder, observe only (mcx::ladder_stats), since the server started and in every
-- pgrust.memory_ladder mode. pgrust_memory_ladder_stats: kind 'ladder' (the mode and line the memory runtime published), 'gate' (draw, admit, scratch, small_draw, load, sample,
-- hook, block (D4812 (2)'s block gate): misses, waits, wait ns, refusals, LastTry ends, climbs), 'rung' (R1 R2a R3 R4a R4b R5 R2b R6: runs, measured
-- relief, ns), 'cancel' (watchdog, ladder_r6, terminate, r6_ack_timeout, projection_tick, projection_inline), 'pick' (D4812: the R6
-- cancel pick's stale_pick, full_pass_age_at_pick, residency_hint, sample_hint, sample_hint_last_episode), 'block_gate' (D4812 (2):
-- small, skipped, busy: runs; sample_wake: runs, ns / wait_max_ns the sample-to-waiter lag) and 'cachefloor' (ladder C1,
-- cache_floor: low / high / room / burst / batch / stuck and per cache <cache>.floor / .held / .pinned in line_bytes; background
-- and inline reclaims: runs, relief_bytes, ns; refill_closed: refusals; giveback: the timed reclaims' runs, bytes, ns;
-- giveback_ns_per_gib: ns, the rate LOW is sized with; C1b: <cache>.budget, the ladder's budget for the part cache, fp
-- tables and condition cache, NULL at its start; C1c: top = HIGH + 5 %, <cache>.raises (runs, relief_bytes = bytes raised
-- past TOP), <cache>.reversals (runs: takes within 10 s of a raise), <cache>.takes (runs, relief_bytes: the ladder's
-- takes from it); part_cache.trims: runs, relief_bytes, climbs = parts; part_cache.evicted_real (D4802): the trims that
-- took bytes out of the census, runs + relief_bytes, a part's faces once as they leave it; part_cache.faces: the faces
-- tier's runs + relief_bytes (what part_cache.trims counts for faces, held faces again each trim); part_cache.loads:
-- relief_bytes = bytes into the census since start; shared_buffers: the ladder's takes of the pool (L6: runs,
-- relief_bytes); shared_buffers.grows (buf_grow): its grows of the pool into free memory (runs, relief_bytes = bytes
-- scheduled);
-- load_gate: the part cache's load gate: misses = loads that asked past the budget, waits / wait_ns = their waits for
-- other statements' pins, refusals = waits that ran out at bytes_wait_ms, climbs = loads that went on past the budget,
-- relief_bytes = bytes their trims evicted, runs = loads the floor held (C1b: cushion only, no wait))
-- and 'fair' (ladder 1.3-1.4, fair_share: level = runs the samples, climbs the queries over their share, line_bytes the
-- max-min level, NULL when all fit; lifts = runs the cuts lifted, relief_bytes the participants' uncuts); the episodes: the last
-- 256 gate waits and hook episodes with the rungs that ran and the outcome; revoke: asks, give-backs and the two REVOKESLA
-- clocks (ask -> stopped growing, ask -> bytes given) as log2-ms histograms with p50 / p99.
CREATE OR REPLACE FUNCTION pgrust_memory_ladder_stats(
    OUT kind text, OUT name text, OUT misses int8, OUT waits int8, OUT wait_ns int8, OUT wait_max_ns int8,
    OUT refusals int8, OUT last_tries int8, OUT climbs int8, OUT runs int8, OUT relief_bytes int8, OUT ns int8,
    OUT cancels int8, OUT line_bytes int8)
RETURNS SETOF record LANGUAGE internal VOLATILE AS 'pgrust_memory_ladder_stats';
CREATE OR REPLACE VIEW pgrust_memory_ladder AS SELECT * FROM pgrust_memory_ladder_stats();
CREATE OR REPLACE FUNCTION pgrust_memory_ladder_episodes(
    OUT seq int8, OUT started timestamptz, OUT ended timestamptz, OUT wait_us int8, OUT site text, OUT need_bytes int8,
    OUT read_bytes int8, OUT line_bytes int8, OUT rungs text, OUT relief_bytes int8, OUT outcome text)
RETURNS SETOF record LANGUAGE internal VOLATILE AS 'pgrust_memory_ladder_episodes';
CREATE OR REPLACE VIEW pgrust_memory_ladder_episodes AS SELECT * FROM pgrust_memory_ladder_episodes();
CREATE OR REPLACE FUNCTION pgrust_memory_revoke_stats(
    OUT asks int8, OUT asked_bytes int8, OUT give_backs int8, OUT given_bytes int8, OUT settled int8,
    OUT stopped_samples int8, OUT stopped_none int8, OUT stopped_p50_ms int8, OUT stopped_p99_ms int8,
    OUT stopped_log2 int8[], OUT given_samples int8, OUT given_none int8, OUT given_p50_ms int8, OUT given_p99_ms int8,
    OUT given_log2 int8[])
RETURNS SETOF record LANGUAGE internal VOLATILE AS 'pgrust_memory_revoke_stats';
CREATE OR REPLACE VIEW pgrust_memory_revoke AS SELECT * FROM pgrust_memory_revoke_stats();
-- cache_track (cache plan C2, 9080): each tracked cache's cost curve, under PGRUST_MEMPOLICY=cache_track=1 (no rows off).
CREATE OR REPLACE FUNCTION pgrust_memory_cache_curves(
    OUT cache text, OUT size_bytes int8, OUT samples int8, OUT refs int8, OUT rate_ppm int8, OUT door int8,
    OUT loss0_ns_per_s int8, OUT v_ns_per_s_per_gib int8, OUT knee_bytes int8, OUT floor_knee_bytes int8, OUT kappa text,
    OUT hull text, OUT dropped int8, OUT own_bytes int8, OUT track_bytes int8, OUT raw_ns_per_s text, OUT open_window text,
    OUT validated boolean, OUT standin text, OUT open_hist text)
RETURNS SETOF record LANGUAGE internal VOLATILE AS 'pgrust_memory_cache_curves';
CREATE OR REPLACE VIEW pgrust_memory_cache_curves AS SELECT * FROM pgrust_memory_cache_curves();
-- S1 criterion 11 (9081): every memory setting with its value now, its default and where the value came from. kind 'knob':
-- every PGRUST_MEMPOLICY key (the policy's one table: a new knob is a new row) and trace, source default / PGRUST_MEMPOLICY /
-- set; kind 'env': the memory switches their own variables set (depot_fetch, the purges, reuse before release, the
-- allocator and Hearth's tenants, ...), read by the code's own readers, source default / environment / set; kind 'guc':
-- the memory GUCs from pg_settings (pgrust.memory_* and pgrust.pgrc_part_cache_mb; value and default with their unit),
-- source pg_settings.source.
CREATE OR REPLACE FUNCTION pgrust_memory_settings(
    OUT kind text, OUT name text, OUT value text, OUT default_value text, OUT source text, OUT set_by text, OUT doc text)
RETURNS SETOF record LANGUAGE internal VOLATILE AS 'pgrust_memory_settings';
CREATE OR REPLACE VIEW pgrust_memory_settings AS
    SELECT * FROM pgrust_memory_settings()
    UNION ALL
    SELECT 'guc', s.name, s.setting || COALESCE(' ' || s.unit, ''), s.boot_val || COALESCE(' ' || s.unit, ''), s.source, s.name,
        s.short_desc
    FROM pg_settings s WHERE s.name LIKE 'pgrust.memory%' OR s.name = 'pgrust.pgrc_part_cache_mb';

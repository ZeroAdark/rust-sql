//! The hosts' run-time inputs home (rule C8): the GUC reads
//! the scan hosts make at run time, moved as is from ops/scan (heap_window.rs's read-ahead sizes, heap_source.rs's
//! bulk-read ring, bitmap_source.rs's TIDBitmap budget). hosts cannot call exec's inputs home (exec depends on hosts), so
//! this is the hosts' own. Plain run-time readers: each reads on the thread and at the point its caller always did (the
//! leader's bind reads the session's); none is a recorded program input or a cached value. worker_arm.rs keeps the worker
//! carry. The placeholder custom options (`pgrust.gravity_*`) that hosts and exec read by name, moved as is from
//! dict/text_hosts.rs: `custom_guc_u64`, `custom_guc_text`.

/// effective_io_concurrency (heap_window.rs `read_ahead`: 0 = the look-ahead off), 0 when not installed (the hosts rig).
pub fn effective_io_concurrency() -> i32 {
    guc(&guc_tables::vars::effective_io_concurrency, 0)
}

/// shared_buffers in blocks (heap_window.rs `limits`, before the buffer pool counts them), 0 when not installed.
pub fn nbuffers() -> i32 {
    guc(&guc_tables::vars::NBuffers, 0)
}

/// max_connections (heap_window.rs `limits`), 1 when not installed.
pub fn max_connections() -> i32 {
    guc(&guc_tables::vars::MaxConnections, 1)
}

/// PostgreSQL's combined read in blocks: min(io_combine_limit, io_max_combine_limit), each at least 1 (1 when not
/// installed), so the result is at least 1 (heap_window.rs `advice_piece`, heap_source.rs `bulk_read_ring`).
pub fn io_combine_blocks() -> i32 {
    guc(&guc_tables::vars::io_combine_limit_guc, 1).min(guc(&guc_tables::vars::io_max_combine_limit, 1))
}

/// PostgreSQL's read stream sizes on the session's settings: (`io_combine_blocks`, `effective_io_concurrency`); a process
/// without them installed (a host test) takes their boot values, io_combine_limit 16 blocks (128 kB) and
/// DEFAULT_EFFECTIVE_IO_CONCURRENCY (the pgrc granule source's `read_knobs`).
pub fn read_stream_sizes() -> (i32, i32) {
    let set = guc_tables::vars::io_combine_limit_guc.installed() && guc_tables::vars::effective_io_concurrency.installed();
    if set { (io_combine_blocks(), effective_io_concurrency()) } else { (16, guc_tables::consts::DEFAULT_EFFECTIVE_IO_CONCURRENCY) }
}

/// The widest width a parallel statement of this server runs at: `max_parallel_workers` within the driver's `MAX_WIDTH`
/// (the bound the executor's grant clamps every session's `max_parallel_workers_per_gather` to, exec inputs/knobs/room.rs
/// `granted_width`); `None` where it is not installed (a host test). facts.rs `width` (`Facts::width`): a lowering rule
/// reads it on a program miss, a decompose rule on a front miss (exec inputs/front.rs keys it), and the program it
/// lowers serves every session of the server (the process-wide program tier keys by the graph, not by a session's
/// width), so the rule reads the server's bound, never one session's `max_parallel_workers_per_gather`.
pub fn parallel_width() -> Option<u16> {
    use guc_tables::vars::max_parallel_workers;
    if !max_parallel_workers.installed() {
        return None;
    }
    u16::try_from(max_parallel_workers.read().clamp(0, i32::from(gravity_driver::MAX_WIDTH))).ok()
}

/// D3047: whether this session's statements run on the leader alone: `max_parallel_workers_per_gather` 0, or a server
/// bound (`parallel_width`) of 0 (exec inputs/knobs/room.rs `granted_width` reads the same settings); `None` where they
/// are not installed (a host test). exec inputs/front.rs keys it beside the server's width, so a decompose rule that
/// reads it (`Facts::serial`: grouping sets' one-table form vs the spool, ops/agg/gsets_tag.rs) never serves one
/// session's choice to a session of the other class; the program tier keys by graph, so equal graphs still share.
pub fn serial_session() -> Option<bool> {
    use guc_tables::vars::max_parallel_workers_per_gather;
    if !max_parallel_workers_per_gather.installed() {
        return None;
    }
    Some(max_parallel_workers_per_gather.read() <= 0 || parallel_width() == Some(0))
}

pub fn condition_cache_mode() -> guc_tables::ConditionCacheMode {
    guc_tables::condition_cache_mode()
}

pub fn condition_cache_on() -> bool {
    condition_cache_mode() != guc_tables::ConditionCacheMode::Off
}

/// `pgrust.zone_store` (boot on): off, a statement neither reads nor writes the cross-statement zone store
/// (granule_source `zone_lookup` / `zone_store`). Read on the session's thread, where every zone ask runs (a source's
/// bind, exec's prepare; a claim carries the session's values to a pool thread).
pub fn zone_store_on() -> bool {
    guc_tables::backing::pgrust_zone_store()
}

/// `pgrust.dict_filter_cache` (boot on): off, no mode keeps a part's per-dictionary-code filter tables across statements
/// (`Resolved::new`); on, the condition cache's mode decides (rows and full keep them).
pub fn dict_filter_cache_on() -> bool {
    guc_tables::backing::pgrust_dict_filter_cache()
}

pub fn startup_prewarm_on() -> bool {
    guc_tables::backing::pgrust_gravity_prewarm()
}

/// work_mem in kB (bitmap_source.rs `build_bitmap`: the TIDBitmap's budget, PostgreSQL's).
pub fn work_mem_kb() -> i32 {
    init_small::globals::work_mem()
}

/// A size knob in bytes (the pgrc granule source's read sizes, `read_knobs` / `advise_knobs`): the custom GUC `guc` in kB
/// when it is set and parses, else the environment variable `env` in kB, else `default` kB. The GUC is read only when
/// the GUC seam is installed (not in the hosts rig).
pub fn knob_kb(guc: &str, env: &str, default: u64) -> u64 {
    let from_guc = if guc_seams::get_config_option_missing_ok::is_installed() { guc_seams::get_config_option_missing_ok::call(guc).ok().flatten().and_then(|v| v.trim().parse::<u64>().ok()) } else { None };
    from_guc.or_else(|| gravity_driver::env::num::<u64>(env)).unwrap_or(default) << 10
}

/// D2985: `PGRUST_GRAVITY_PART_RELEASE=0` keeps every statement's pgrc dictionaries in the list's shared cells (the kill
/// switch of granule_source/release.rs; read once).
pub fn part_release_enabled() -> bool {
    release_switches().0
}

/// D3823: `PGRUST_GRAVITY_RELEASE_DICTS=0` keeps a uses-only release's dictionaries in the list's shared cells to the
/// statement's end, as before (the kill switch of `Resolved::set_part_own_dicts`; read once).
pub fn release_dicts_enabled() -> bool {
    release_switches().1
}

/// The three release kill switches, read once together.
fn release_switches() -> (bool, bool, bool) {
    env_levers().0
}

/// The env levers read once together (nothing in the process sets them): the three release kill switches,
/// `PGRUST_GRAVITY_PART_CACHE_MB` (`part_cache_server_mb`), `PGRUST_GRAVITY_METACACHE` (`metacache_enabled`),
/// `PGRUST_GRAVITY_RENDER_PARK` (`render_park_enabled`) and `PGRUST_GRAVITY_PART_DECIDES_MEMO` (`part_decides_memo`).
fn env_levers() -> ((bool, bool, bool), Option<u64>, bool, bool, bool) {
    static ON: pgsync::OnceLock<((bool, bool, bool), Option<u64>, bool, bool, bool)> = pgsync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = |name: &str| gravity_driver::env::num::<u32>(name) != Some(0);
        let switches = (on("PGRUST_GRAVITY_PART_RELEASE"), on("PGRUST_GRAVITY_RELEASE_DICTS"), on("PGRUST_GRAVITY_UNPIN_BEHIND"));
        let memo = gravity_driver::env::num::<u32>("PGRUST_GRAVITY_PART_DECIDES_MEMO") == Some(1);
        (switches, gravity_driver::env::num::<u64>("PGRUST_GRAVITY_PART_CACHE_MB"), on("PGRUST_GRAVITY_METACACHE"), on("PGRUST_GRAVITY_RENDER_PARK"), memo)
    })
}

/// D4315: `PGRUST_GRAVITY_UNPIN_BEHIND=0` keeps a releasing scan's whole claim read at its part's entry and every extent
/// it read pinned to the part's release, as before (the kill switch of granule_source's read-ahead and `pass_behind`;
/// read once).
pub fn unpin_behind_enabled() -> bool {
    release_switches().2
}

/// D4504: `PGRUST_GRAVITY_METACACHE=0` keeps a part list's metadata faces out of the part cache's census and lets every
/// list go at a trim that evicted dictionary images, as before (the A/B's off arm of granule_source's `FaceBodies`;
/// read once).
pub fn metacache_enabled() -> bool {
    env_levers().2
}

/// D4554: `PGRUST_GRAVITY_PART_DECIDES_MEMO=1` asks a granule source's D3160 part verdict (`part_decides`: a zone predicate
/// with no granule record, decided by its part's record) once a part, not once a granule (off by default; read once).
pub fn part_decides_memo() -> bool {
    env_levers().4
}

/// D3156: `PGRUST_GRAVITY_KEPT_DICTS=0` keeps every verbatim part's code table whole (the kill switch of
/// granule_source/kept.rs; read once).
pub fn kept_dicts_enabled() -> bool {
    static ON: pgsync::OnceLock<bool> = pgsync::OnceLock::new();
    *ON.get_or_init(|| gravity_driver::env::num::<u32>("PGRUST_GRAVITY_KEPT_DICTS") != Some(0))
}

/// D3324 R1: `PGRUST_GRAVITY_RENDER_PARK=0` starts a code lane's renders over at every part switch, as before (the kill
/// switch of text_hosts `RenderMemo`'s parked verbatim parts; read once).
pub fn render_park_enabled() -> bool {
    env_levers().3
}

/// D2985: debug builds, and `PGRUST_GRAVITY_RELEASE_POISON=1`, poison a released dictionary image the statement held alone
/// (granule_source/release.rs; read once).
pub fn release_poison() -> bool {
    static ON: pgsync::OnceLock<bool> = pgsync::OnceLock::new();
    *ON.get_or_init(|| cfg!(debug_assertions) || gravity_driver::env::num::<u32>("PGRUST_GRAVITY_RELEASE_POISON") == Some(1))
}

/// The GUC store's mutation count (pgrc residency's `road_watch`): every SET / RESET / SET LOCAL / set_config and the
/// transaction-end restore move it, so an unchanged count means no option changed since the last look.
pub fn guc_store_mutations() -> u64 {
    guc::store::store_mutation_count()
}

/// An integer GUC's value on this thread (the leader's bind reads the session's), `floor` when not installed (the hosts rig).
fn guc(v: &guc_tables::GucIntVar, floor: i32) -> i32 {
    if v.installed() { v.read().max(floor) } else { floor }
}

/// Memory step 5: the memory broker's settings from its process-global GUCs (`pgrust.memory_broker`,
/// `pgrust.memory_target`, `pgrust.memory_broker_actuators`, `pgrust.memory_page_cache_floor_mb`, `pgrust.memory_reserve_mb`).
/// The e2e levers `PGRUST_GRAVITY_MEMORY_BROKER` (off / advise / on), `PGRUST_GRAVITY_MEMORY_TARGET_MB`,
/// `PGRUST_GRAVITY_MEMORY_BROKER_ACTUATORS` and `PGRUST_GRAVITY_MEMORY_RESERVE_MB` win when set: the grant pool arms at the
/// harness's probe, before a file's statements can set anything (an e2e file's .env, D622).
pub fn memory_budget_cfg() -> gravity_driver::mem::budget::Cfg {
    use gravity_driver::mem::budget::{actuator, parse_actuators, Cfg, Mode};
    let mb = |v: i32| (v.max(0) as u64) << 20;
    let mode = match gravity_driver::env::text("PGRUST_GRAVITY_MEMORY_BROKER").as_deref().map(str::trim) {
        Some("off") => Mode::Off,
        Some("advise") => Mode::Advise,
        Some("on") => Mode::On,
        _ => Mode::from_index(guc_tables::backing::pgrust_memory_broker()),
    };
    let target = gravity_driver::env::num::<u64>("PGRUST_GRAVITY_MEMORY_TARGET_MB").map_or_else(|| mb(guc_tables::backing::pgrust_memory_target()), |m| m << 20);
    let actuators = gravity_driver::env::text("PGRUST_GRAVITY_MEMORY_BROKER_ACTUATORS")
        .or_else(guc_tables::backing::pgrust_memory_broker_actuators)
        .map_or(actuator::ALL, |s| parse_actuators(&s).0);
    Cfg {
        mode,
        target,
        page_cache_floor: mb(guc_tables::backing::pgrust_memory_page_cache_floor_mb()),
        reserve: gravity_driver::env::num::<u64>("PGRUST_GRAVITY_MEMORY_RESERVE_MB").map_or_else(|| mb(guc_tables::backing::pgrust_memory_reserve_mb()), |m| m << 20),
        actuators,
    }
}

/// D3675: `pgrust.pgrc_part_cache_mb`, the pgrc part cache's budget in MB for the whole server (PGC_SIGHUP: a session SET
/// is refused with PostgreSQL's 55P02, so every session reads one value). None = derived (-1, or no GUC store: the hosts
/// rig). The e2e lever `PGRUST_GRAVITY_PART_CACHE_MB` wins when set (an e2e file's .env: the server's value; read once,
/// as nothing in the process sets it: every statement's part-cache arm asks, and a getenv per statement was 0.6-0.9k
/// instructions).
pub fn part_cache_server_mb() -> Option<u64> {
    if let Some(mb) = env_levers().1 {
        return Some(mb);
    }
    let v = &guc_tables::vars::pgrust_pgrc_part_cache_mb;
    if !v.installed() {
        return None;
    }
    u64::try_from(v.read()).ok()
}

/// C3: `pgrust.condition_cache_size` as set, in kB (`None`: not installed, or -1 = the derived budget), read process-wide
/// (no GUC store: memruntime's cache purger reads it).
pub fn condition_cache_kb() -> Option<u64> {
    let v = &guc_tables::vars::pgrust_condition_cache_size;
    if !v.installed() {
        return None;
    }
    u64::try_from(v.read()).ok()
}

/// D3793: shared_buffers in bytes (NBuffers pages of 8 kB); 0 without a GUC store (the hosts rig).
pub fn shared_buffers_bytes() -> u64 {
    let n = &guc_tables::vars::NBuffers;
    if n.installed() { (n.read().max(0) as u64).saturating_mul(8192) } else { 0 }
}

/// D3793: the larger of the grant pool's explicit size (`pgrust.gravity_grant_pool_mb`, else the e2e lever
/// `PGRUST_GRAVITY_GRANT_POOL_MB`, as exec's pool arms) and a statement's explicit grant (`pgrust.gravity_grant_mb`), in
/// MB; 0 when both are derived.
pub fn grant_explicit_mb() -> u64 {
    let pool = custom_guc_u64("pgrust.gravity_grant_pool_mb").or_else(|| gravity_driver::env::num::<u64>("PGRUST_GRAVITY_GRANT_POOL_MB"));
    pool.unwrap_or(0).max(custom_guc_u64("pgrust.gravity_grant_mb").unwrap_or(0))
}

/// The memory policy's override (`PGRUST_MEMPOLICY`, `mempolicy::consts`), read once at the postmaster's start; None when
/// unset. An environment variable only: nothing about it is visible to SQL.
pub fn mempolicy_override() -> Option<String> {
    gravity_driver::env::text("PGRUST_MEMPOLICY")
}

/// D3793: `pgrust.pgrc_part_cache_mb`'s check hook: `memory_budget::part_cache_refusal` names a refusal's DETAIL and
/// HINT; the GUC machinery then LOGs it and keeps the old value on a reload, or raises it at ALTER SYSTEM.
pub fn install_part_cache_check() {
    guc_tables::hooks::check_pgrust_pgrc_part_cache_mb.install(|v, _extra, _source| match crate::memory_budget::part_cache_refusal(*v) {
        None => Ok(true),
        Some((detail, hint)) => {
            guc_seams::guc_check_errdetail::call(detail);
            guc_seams::guc_check_errhint::call(hint);
            Ok(false)
        }
    });
}

/// Memory step 6a: `pgrust.memory_broker_interval` in seconds, at least 1 (process-global, read on the
/// memory watchdog's thread); the e2e lever `PGRUST_GRAVITY_MEMORY_BROKER_INTERVAL_S` wins when set.
pub fn memory_broker_interval_s() -> u64 {
    gravity_driver::env::num::<u64>("PGRUST_GRAVITY_MEMORY_BROKER_INTERVAL_S")
        .unwrap_or_else(|| guc_tables::backing::pgrust_memory_broker_interval().max(1) as u64)
        .max(1)
}

/// A placeholder custom option (`SET` / `postgresql.conf`) as a number, `None` when unset or unparsable.
pub fn custom_guc_u64(name: &str) -> Option<u64> {
    if !guc_seams::get_config_option_missing_ok::is_installed() {
        return None;
    }
    guc_seams::get_config_option_missing_ok::call(name).ok().flatten().and_then(|v| v.trim().parse::<u64>().ok())
}

/// A placeholder custom option as text, trimmed; `None` when unset or empty.
pub fn custom_guc_text(name: &str) -> Option<String> {
    if !guc_seams::get_config_option_missing_ok::is_installed() {
        return None;
    }
    guc_seams::get_config_option_missing_ok::call(name).ok().flatten().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) // alloc: Cold
}

/// Where a reader that reads the same option after every GUC store mutation found its name in the registry
/// (`custom_guc_text_at` / `custom_guc_u64_at`): one per option and thread, in a `thread_local`.
pub use guc::OptionSlot;

/// `custom_guc_text` through the option's slot: the name is looked up in the registry once and read by its index after,
/// so a knob read again after an unrelated SET (a function writing its own option on every call) costs no lookup by
/// name (D2896). A thread with no GUC store reads through the seam as `custom_guc_text` does.
pub fn custom_guc_text_at(slot: &core::cell::Cell<OptionSlot>, name: &str) -> Option<String> {
    raw_at(slot, name).map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) // alloc: Cold
}

/// `custom_guc_u64` through the option's slot (`custom_guc_text_at`).
pub fn custom_guc_u64_at(slot: &core::cell::Cell<OptionSlot>, name: &str) -> Option<u64> {
    raw_at(slot, name).and_then(|v| v.trim().parse::<u64>().ok())
}

fn raw_at(slot: &core::cell::Cell<OptionSlot>, name: &str) -> Option<String> {
    if !guc_seams::get_config_option_missing_ok::is_installed() {
        return None;
    }
    match guc::get_config_option_at(slot, name) {
        Some(v) => v,
        None => guc_seams::get_config_option_missing_ok::call(name).ok().flatten(),
    }
}

/// The cluster's system identifier (pg_control); `None` before this process read the control file (a rig).
pub fn system_identifier() -> Option<u64> {
    transam_xlog::control_file::control_file_loaded().then(transam_xlog::control_file::GetSystemIdentifier)
}

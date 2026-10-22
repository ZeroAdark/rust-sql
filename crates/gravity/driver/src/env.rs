//! The one reader of the process environment behind Gravity's `PGRUST_GRAVITY_*` knobs (measurement and debug
//! levers, never product surface; exec's `inputs::knobs` holds their table). Every crate at or above the driver reads
//! an env knob through here, so the knobs are one grep (gravity_lint `env_reads`); callers read once and cache.
//! The crates below exec (the code backend) read their run-time options through the GUC seam here too: they reach no
//! lowering or exec inputs home.

/// The raw value, `None` when unset.
pub fn os(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(name)
}

/// Set at all (any value, even empty).
pub fn set(name: &str) -> bool {
    os(name).is_some()
}

/// The value as text, `None` when unset or not UTF-8.
pub fn text(name: &str) -> Option<String> {
    std::env::var(name).ok() // alloc: Cold
}

/// The value parsed after trimming blanks, `None` when unset or unparsable.
pub fn num<T: core::str::FromStr>(name: &str) -> Option<T> {
    text(name)?.trim().parse().ok()
}

/// The GUC seam is installed (a server process; unit tests and tools run without it).
pub fn guc_installed() -> bool {
    guc_seams::get_config_option_missing_ok::is_installed()
}

/// An option's value parsed after trimming blanks, read at run time through the GUC seam; `None` when the seam is not
/// installed, the option is missing or its value does not parse.
pub fn guc_num<T: core::str::FromStr>(name: &str) -> Option<T> {
    guc_installed().then(|| guc_seams::get_config_option_missing_ok::call(name).ok().flatten().and_then(|v| v.trim().parse::<T>().ok())).flatten()
}

/// pglserve (D4601): the serve-path cuts' switch, `PGRUST_GRAVITY_SERVE_LEAN` (default on; exactly `0` = the old paths:
/// a hosted aggregate's 64 home shards made at its arena's start, a parallel run's census read every statement). Both
/// paths give the same rows, errors and charges; only the instructions differ. Read once per process.
pub fn serve_lean() -> bool {
    static ON: pgsync::OnceLock<bool> = pgsync::OnceLock::new();
    *ON.get_or_init(|| num::<u32>("PGRUST_GRAVITY_SERVE_LEAN") != Some(0))
}

/// D4690 (a debug lever, off unless exactly `1`): `PGRUST_MAKE_ROOM_LOG=1` logs one line per inline `make_room` call
/// (`mem::reclaim`: the draw, B's free bytes, what it asked, the bytes each rung and cache freed, the time). Read once per
/// process.
pub fn make_room_log() -> bool {
    static ON: pgsync::OnceLock<bool> = pgsync::OnceLock::new();
    *ON.get_or_init(|| num::<u32>("PGRUST_MAKE_ROOM_LOG") == Some(1))
}

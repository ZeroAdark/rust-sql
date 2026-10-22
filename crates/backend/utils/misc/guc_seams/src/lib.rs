use types_error::PgResult;

seam_core::seam!(
    pub fn new_guc_nest_level() -> i32
);

seam_core::seam!(
    pub fn at_eoxact_guc(is_commit: bool, nest_level: i32) -> PgResult<()>
);

seam_core::seam!(
    // fmgr_security_definer's proconfig loop (fmgr.c:740-753): each
    // "name=value" entry applied at superuser()?PGC_SUSET:PGC_USERSET,
    // PGC_S_SESSION, GUC_ACTION_SAVE, changeVal=true.
    pub fn process_guc_array_secdef<'a>(array: &'a [String]) -> PgResult<()>
);

seam_core::seam!(
    // SetConfigOption(name, value, PGC_INTERNAL, PGC_S_DYNAMIC_DEFAULT) (guc.c);
    // miscinit's SetOuterUserId keeps the is_superuser GUC in sync through it.
    pub fn set_config_option_internal_dynamic_default(name: &str, value: &str) -> PgResult<()>
);

seam_core::seam!(
    // SetConfigOption(name, value, context, source) (guc.c); miscinit's
    // InitializeSessionUserId sets session_authorization at PGC_S_OVERRIDE
    // through it (guc depends on miscinit, so no direct edge back).
    pub fn set_config_option(
        name: &str,
        value: Option<&str>,
        context: types_guc::GucContext,
        source: types_guc::GucSource,
    ) -> PgResult<()>
);

seam_core::seam!(
    // GUC_check_errdetail (guc.c).
    pub fn guc_check_errdetail(detail: String)
);

seam_core::seam!(
    // GUC_check_errcode (guc.c): the SQLSTATE a check hook's `return false`
    // reports instead of ERRCODE_INVALID_PARAMETER_VALUE.
    pub fn guc_check_errcode(sqlstate: types_error::SqlState)
);

seam_core::seam!(
    // GUC_check_errhint (guc.c).
    pub fn guc_check_errhint(hint: String)
);

seam_core::seam!(
    // ProcessConfigFileInternal(context, applySettings, elevel) (guc.c); the
    // guc-file.l wrapper reaches back across the guc <-> guc-file cycle.
    pub fn process_config_file_internal(
        context: types_guc::GucContext,
        apply_settings: bool,
        elevel: types_error::ErrorLevel,
    ) -> PgResult<()>
);

seam_core::seam!(
    // SelectConfigFiles(userDoption, progname) (guc.c) — deferred half of the
    // ported guc unit; false = C's "exit(2)" failure return.
    pub fn select_config_files(user_d_option: Option<&str>, progname: &str) -> PgResult<bool>
);

seam_core::seam!(
    // InitializeGUCOptions (guc.c) — same deferred half.
    pub fn initialize_guc_options() -> PgResult<()>
);


seam_core::seam!(
    // get_explain_guc_options (guc.c): (name, GetConfigOptionByName value)
    // for GUC_EXPLAIN vars modified from boot; EXPLAIN (SETTINGS) reads it
    // (installed by guc_funcs, which owns the ConfigOptionIsVisible deps).
    pub fn get_explain_guc_options() -> PgResult<Vec<(String, Option<String>)>>
);

seam_core::seam!(
    // GetConfigOption(name, missing_ok=true, restrict_privileged=false)
    // (guc.c) — lets low-level crates (planner costing, executor arming)
    // read placeholder customized options (e.g. `pgrust.lane_parallel_pool`)
    // without a dependency edge onto the full guc crate. None = never set.
    pub fn get_config_option_missing_ok(name: &str) -> PgResult<Option<String>>
);

/// The `pgrust.*` placeholder options a per-statement path reads (no record of their own: an option is absent until a SET
/// makes its placeholder). By name each read was a GetConfigOption: a store borrow, a name hash and a String, 1.3-2.3k
/// instructions per statement over the planner's three. As a knob, the guc crate reads the option once per change of
/// this thread's store and keeps how its text reads (`knob_read`): two thread-local loads per read after that.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Knob {
    /// `pgrust.gravity_width_costing` (planner gravity_width_cost.rs level_setup).
    GravityWidthCosting,
    /// `pgrust.gravity_width_costing_trace` (planner gravity_width_cost.rs trace).
    GravityWidthCostingTrace,
    /// `pgrust.gravity_decorrelate_ctes` (planner decorrelate.rs cte_walk).
    GravityDecorrelateCtes,
    /// `pgrust.gravity_decorrelate_or_factor` (planner decorrelate.rs or_factored).
    GravityDecorrelateOrFactor,
    /// `pgrust.gravity_unnest_reach` (planner unnest.rs reach).
    GravityUnnestReach,
    /// `pgrust.gravity_unnest_args` (planner unnest.rs args_reach).
    GravityUnnestArgs,
}

impl Knob {
    /// How many knobs there are: the guc crate's per-thread readings are an array indexed by `self as usize`.
    pub const COUNT: usize = 6;

    /// The option's name.
    pub const fn name(self) -> &'static str {
        match self {
            Knob::GravityWidthCosting => "pgrust.gravity_width_costing",
            Knob::GravityWidthCostingTrace => "pgrust.gravity_width_costing_trace",
            Knob::GravityDecorrelateCtes => "pgrust.gravity_decorrelate_ctes",
            Knob::GravityDecorrelateOrFactor => "pgrust.gravity_decorrelate_or_factor",
            Knob::GravityUnnestReach => "pgrust.gravity_unnest_reach",
            Knob::GravityUnnestArgs => "pgrust.gravity_unnest_args",
        }
    }

    /// The words a reader tells apart, matched without ASCII case (what its callers did with to_ascii_lowercase), and
    /// whether the text is trimmed of blanks first. One list per knob, so every reader of a knob reads it alike.
    pub const fn words(self) -> (&'static [&'static str], bool) {
        const ON: &[&str] = &["on", "true", "yes", "1"];
        const CTE_WALK: &[&str] = &["off", "false", "0", "on", "true", "1", "all", "materialized"];
        const OFF_ON: &[&str] = &["off", "false", "0", "no", "on", "true", "1", "yes"];
        const REACH: &[&str] = &["base", "p2", "wide"];
        match self {
            Knob::GravityWidthCosting | Knob::GravityWidthCostingTrace => (ON, false),
            Knob::GravityDecorrelateCtes => (CTE_WALK, true),
            Knob::GravityDecorrelateOrFactor | Knob::GravityUnnestArgs => (OFF_ON, true),
            Knob::GravityUnnestReach => (REACH, true),
        }
    }

    /// How `text` (the option's value, `None` when absent) reads: the first of `words` it equals, or `Other`.
    pub fn read(self, text: Option<&str>) -> KnobRead {
        let Some(text) = text else { return KnobRead::Unset };
        let (words, trim) = self.words();
        let text = if trim { text.trim() } else { text };
        match words.iter().position(|w| text.eq_ignore_ascii_case(w)) {
            Some(i) => KnobRead::Word(i as u8),
            None => KnobRead::Other,
        }
    }
}

/// How a knob's text reads (`Knob::read`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KnobRead {
    /// The option is absent (never set in this session: no placeholder).
    Unset,
    /// Its text is the knob's word at this index (`Knob::words`).
    Word(u8),
    /// Its text is none of the knob's words.
    Other,
}

seam_core::seam!(
    // A knob's reading on this thread: Knob::read of GetConfigOption(knob.name(), missing_ok=true,
    // restrict_privileged=false), kept by the guc crate until this thread's store changes.
    pub fn knob_read(knob: Knob) -> KnobRead
);

//! The admission-order and lowerer knobs (exec.md "Admission from the graph"; lowerv2-plan §4.3): the guc_tables enums read here, and the per-statement marks the admission reads (nested, explain-only, instrumented, publishing).

use core::cell::Cell;

use super::{Kind, Knob};

pub static KNOBS: &[Knob] = &[
    Knob { name: "pgrust.gravity_admit", kind: Kind::Builtin, default: "graph", doc: "graph = decompose the plan and admit from its graph; shape = graph admission off: no shape table is left, so every statement is refused by name (no fallback executor)" },
    Knob { name: "pgrust.gravity_lowerer", kind: Kind::Builtin, default: "auto", doc: "auto = the registered lowerer (none until lowerv4 M0: every graph refused by name, lowerer v4 not built yet) | v1, v2, v3 removed (tags ladder-final, lowerv2-final, lowerv3-final)" },
    Knob { name: crate::admit::lean::SWITCH, kind: Kind::Env, default: "unset (on)", doc: "D4600: 0 = every admission steady-state cut (admit/lean.rs) back on its former road (the cuts' internal A/B switch; any other value or unset = on, read once)" },
    Knob { name: LIST_SALT_ENV, kind: Kind::Env, default: "unset (off)", doc: "D4786: 1 = a program that read a class-keyed list's image (tier=miss:no_cache.list, never filed under its content key) is filed under the key salted with the graph's class-keyed lists' bytes (admit list_salted), in its backend (tier=local:list-filed, then local:list) and, as any other program, process-wide (published:list, then hit:list), so a warm session stops lowering it at every execution and a new session at its first" },
    Knob { name: RELSET_GUC, kind: Kind::Custom, default: "on", doc: "D3585: on = an Append's runs of twin heap scans (one layout, the same clauses) read as one relation-set pipeline each; off = one pipeline per arm" },
];

/// D4786: the list-salted filing's switch (`list_salt_on`).
pub const LIST_SALT_ENV: &str = "PGRUST_GRAVITY_LIST_SALT";

/// D4786: `PGRUST_GRAVITY_LIST_SALT=1` files a never-filed program that read a class-keyed list's image under the lists'
/// bytes, in its backend and process-wide (admit `list_salted`); default off (read once).
pub fn list_salt_on() -> bool {
    static ON: pgsync::OnceLock<bool> = pgsync::OnceLock::new();
    *ON.get_or_init(|| gravity_driver::env::num::<u32>(LIST_SALT_ENV).is_some_and(|v| v != 0))
}

/// D3585: the relation-set grouping's switch (`relset_on`).
pub const RELSET_GUC: &str = "pgrust.gravity_relset";

pub const GUC: &str = "pgrust.gravity_admit";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Shape,
    Graph,
}

/// `pgrust.gravity_admit`: `graph` (the default) = admit from the graph; `shape` = every statement refused by name. The GUC is an enum (`guc_tables`), so only those two spellings reach here; a backend without the GUC machinery takes the default.
pub fn mode() -> Mode {
    let v = &guc_tables::vars::pgrust_gravity_admit;
    if v.installed() && v.read() == guc_tables::consts::GRAVITY_ADMIT_SHAPE {
        Mode::Shape
    } else {
        Mode::Graph
    }
}

/// `pgrust.gravity_lowerer`'s check hook: `v1` (the rung ladder), `v2` (lowerv2) and `v3` (lowerv3) are removed from the build,
/// an ERROR at SET. No lowerer is registered (`gravity_lower::set_lowerer`): lowerv4's M0 registers its `lower_graph` here.
pub(crate) fn install_lowerer_check() {
    gravity_lower::set_lowerer(gravity_lower::LowererHook { lower: gravity_lowerv4::lower_graph });
    gravity_lowerv4::frame::diag::set(super::diag::all_blockers());
    gravity_lowerv4::frame::program::set_gate_stamps(super::diag::ahead_trace());
    gravity_lowerv4::dict::face::set_front_runs(super::diag::front_runs());
    gravity_lowerv4::ops::hashjoin::face::set_semi_keys(super::diag::semi_keys());
    guc_tables::hooks::check_pgrust_gravity_lowerer.install(|v, _extra, _source| {
        let removed = match *v {
            guc_tables::consts::GRAVITY_LOWERER_V1 => "ladder removed; see tag ladder-final",
            guc_tables::consts::GRAVITY_LOWERER_V2 => "lowerv2 removed; see tag lowerv2-final",
            guc_tables::consts::GRAVITY_LOWERER_V3 => "lowerv3 removed; see tags lowerv3-final, lowerv3-landed",
            _ => return Ok(true),
        };
        guc_seams::guc_check_errdetail::call(removed.to_string()); // alloc: Cold
        Ok(false)
    });
}

leader_tls::leader_tls! {
    thread_local! {
        /// `RELSET_GUC`'s registry slot, the GUC store's mutation count `relset_on` last read it at, and what it read.
        static RELSET_SLOT: Cell<gravity_hosts::inputs::OptionSlot> = const { Cell::new(gravity_hosts::inputs::OptionSlot::Unknown) };
        static RELSET_AT: Cell<u64> = const { Cell::new(u64::MAX) };
        static RELSET: Cell<bool> = const { Cell::new(false) };
        static NESTED: Cell<bool> = const { Cell::new(false) };
        // an EXPLAIN without ANALYZE admits for its verdict only (entry.md §13): its refusal is a scoreboard line, never the statement's error
        static EXPLAIN: Cell<bool> = const { Cell::new(false) };
        // an EXPLAIN ANALYZE's admission and open (ENTRY E1): lowered with `LowerOpts::instrument`, past the program cache, leader only
        static INSTRUMENT: Cell<bool> = const { Cell::new(false) };
        // D2798: an identity cursor's admission whose plan calls user code: lowered with `LowerOpts::publish`, filed apart
        static PUBLISH: Cell<bool> = const { Cell::new(false) };
    }
}

pub fn instrumented(v: bool) -> bool {
    INSTRUMENT.with(|c| c.replace(v))
}

/// D3585: `pgrust.gravity_relset` on (unset, `on`, `true`, `yes`, `1`): the admission reads an Append's runs of twin heap
/// scans as relation sets (decompose `admit::grouping_relsets`); any other value (`off`) keeps one pipeline per arm. On by
/// default. Read again only after a GUC store mutation.
pub fn relset_on() -> bool {
    let at = gravity_hosts::inputs::guc_store_mutations();
    if RELSET_AT.with(|c| c.replace(at)) != at {
        let v = RELSET_SLOT.with(|slot| gravity_hosts::inputs::custom_guc_text_at(slot, RELSET_GUC));
        RELSET.with(|c| c.set(relset_reads_on(v.as_deref())));
    }
    RELSET.with(Cell::get)
}

/// D3585: what a `pgrust.gravity_relset` value reads as: unset, `on`, `true`, `yes` or `1` (any case) on; any other off.
fn relset_reads_on(v: Option<&str>) -> bool {
    v.is_none_or(|v| matches!(v.to_ascii_lowercase().as_str(), "on" | "true" | "yes" | "1"))
}

pub fn is_instrumented() -> bool {
    INSTRUMENT.with(Cell::get)
}

pub fn publishing(v: bool) -> bool {
    PUBLISH.with(|c| c.replace(v))
}

pub fn is_publishing() -> bool {
    PUBLISH.with(Cell::get)
}

pub fn nested(v: bool) -> bool {
    NESTED.with(|c| c.replace(v))
}

pub fn is_nested() -> bool {
    NESTED.with(Cell::get)
}

pub fn explain_only(v: bool) -> bool {
    EXPLAIN.with(|c| c.replace(v))
}

pub fn is_explain_only() -> bool {
    EXPLAIN.with(Cell::get)
}

#[cfg(test)]
mod tests {
    /// D3585: relation sets are on unless the option says off: unset reads on, and the census row's default says so.
    #[test]
    fn relation_sets_are_on_unless_the_option_says_off() {
        use super::{relset_reads_on, KNOBS, RELSET_GUC};
        assert!(relset_reads_on(None), "unset: on");
        for v in ["on", "ON", "true", "yes", "1"] {
            assert!(relset_reads_on(Some(v)), "{v}: on");
        }
        for v in ["off", "false", "no", "0", "maybe"] {
            assert!(!relset_reads_on(Some(v)), "{v}: off");
        }
        assert_eq!(KNOBS.iter().find(|k| k.name == RELSET_GUC).map(|k| k.default), Some("on"));
    }
}

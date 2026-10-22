//! `gravity_road_off` (plan §3.3, D255): the roads forced off for twin A/Bs and dev, read at the top of every lowering, and
//! the roads-off log line (moved from lib.rs, main 09-24). Owner M1-CORE.
use crate::face::strategy;

leader_tls::leader_tls! {
    thread_local! {
        /// The forced-off set this backend last logged (`log`).
        static LOGGED_OFF: core::cell::Cell<strategy::StrategySet> = const { core::cell::Cell::new(strategy::StrategySet::EMPTY) };
        /// The roads the option itself names off (`read`): the log line's, never a default-off road the option left alone.
        static NAMED_OFF: core::cell::Cell<strategy::StrategySet> = const { core::cell::Cell::new(strategy::StrategySet::EMPTY) };
    }
}

/// Roads built default off until their gates pass: off in every lowering unless the option names them `+<id>` (`SET
/// pgrust.gravity_road_off = '+<id>'`), so one server serves both arms of an A/B and a witness takes the road; the option's
/// text keys the program tier (D311), so the arms never share a program. A road's flip entry takes it off this list (D3194:
/// `join.rowfilter`, on since its gates passed; D4585: `scan.mergefree`, the narrowed road with its serial keep, on since
/// its go / no-go); `+<id>` on a road already on changes nothing. D4585: `scan.mergefree.build`
/// (hosts `Facts::scan_mergefree_build`), the hash-build half of `scan.mergefree`: without it a hash build's input keeps
/// its mark (the narrowed road), `+scan.mergefree.build` drops it too (S1 as it landed).
/// D3324 R3: `key.carry` (ops/agg/strategies/dependent.rs, the carried form), off until its A/B and the user's call.
/// D3336: `merge.wideemit` and `emit.parmerge` (a merge join's inner Sort filled by a group emit's claims; a sorted wide
/// emit's runs merged by key range in parallel) are on: not on this list, so a plain name turns each off.
/// D4497: `nest.keep.initplan` and `nest.keep.subquery` (decompose roads, read through `Facts::default_on_road_on`,
/// `gravity_decompose::ROADS_DEFAULT_ON`) are on since their flip entry: not on this list, so a plain name turns each
/// off.
/// D4569: `append.nest` (a decompose road: exec admission reads it through ops/append/strategies/nest.rs `on`), a SELECT's
/// quiet union nest copied into the arms, and `join.rtfilter.arms`, a build's runtime filter on every union arm's probe
/// copy, and `drain.spool.wide` (ops/hashjoin/strategies/drain_wide.rs), a spooled drain's quiet probe filling an owned
/// spool at width, are on: not on this list, so a plain name turns each off.
/// D3227: `nest.keep.semi` and `nest.keep.material` (decompose roads, read the same way) are on with D4497's two, the
/// same.
/// D3233: `case.select` (ops/agg/numeric/strategies/num_case.rs, gravity_expr emit/scalar.rs `CaseSelect`) is on: not on
/// this list, so a plain name turns it off.
/// D4565: `agg.stat_reg` (a decompose road read through `Facts::default_on_road_on`,
/// `gravity_decompose::ROADS_DEFAULT_ON`), var / stddev over int2 / int4 in registers, and D4567: `agg.num_scale` (a
/// decompose road read the same way), a numeric sum's decided scale beside a mixed-scale CASE / COALESCE, are on since
/// their flip entry: not on this list, so a plain name turns each off.
/// D3323: `join.rowfilter.single` (frame.rs, an armed filter's body ahead of a later probe's one-row body) and
/// `join.rowfilter.cost` (ops/hashjoin/strategies/rowfilter/cost.rs, the filter's verdict from measured costs, sampled
/// again) are on since their default flip (the user's call): not on this list, so a plain name turns each off.
/// D3168: `join.probe.staged` (ops/hashjoin/strategies/probe_stage.rs, a probe past L2 in stages over a block, chosen by
/// the BUILT table's size since S0) is on since its flip entry (the user's call): not on this list, so a plain name turns
/// it off.
/// D3227 (b): `nest.key.eq` (a decompose road, read through `Facts::default_on_road_on`,
/// `gravity_decompose::ROADS_DEFAULT_ON`) is on since its flip entry: not on this list, so a plain name turns it off.
/// D4676: `group.hashchain` (a decompose road read through `Facts::road_on`, ops/group/absorb.rs `chain_of`), a Sort ->
/// Unique -> sorted GroupAggregate chain served hashed (and an order-free group's direct key Sort absorbed,
/// `group.freesort`), off: it admits only a Unique over a plain Sort, and the planner often dedupes a UNION as a Unique
/// over a Merge Append of per-arm Sorts instead, which it leaves alone.
/// D4628: `spool.seal` (rerun/spool/seal.rs, an owned spool sealed off the leader's copy), off until its A/B and the user's
/// call.
/// textkeys step 1: `text.lean` (rep/varlena/text_lean.rs, text.eq's cheap per-row form) is on since its flip entry:
/// not on this list, so a plain name turns it off.
/// D4620: `agg.gsets.cascade` (a decompose road read through `Facts::default_on_road_on`, lowered by
/// ops/agg/cascade.rs), coarser grouping sets folded from a finer set's records, is on since its flip entry: not on
/// this list, so a plain name turns it off.
/// genreg P6 (§10 item 7): `join.payload_fp` and `join.drain.fp` (ops/hashjoin/strategies/payload_fp.rs, a build's
/// records carrying a text payload's Fp128 words, and a drain linking them) are on: not on this list, so a plain name turns
/// each off (the build keeps the render).
/// textkeys step 2: `text.set` (rep/varlena/text_set.rs, expr.saop's byte set probed in line) is on with text.lean, the
/// same.
/// D4567 (a): `agg.num_div` (a decompose road read through `Facts::road_on`, listed in `gravity_decompose::ROADS`), a
/// numeric sum / avg of `x / k` in registers, off until its A/B and the user's call.
/// textkeys step 3: `key.canon` (rep/varlena/key_canon.rs, one canonical text key image for every source), off until its
/// A/B and the user's call.
/// D3336 (A2 (iii)): `cte.wholefill` (ops/ctescan/wholefill.rs, a CTE that may raise filled whole ahead under v5
/// errorhold's licence, its sorting readers by record claims), off until its A/B and the user's call.
/// D4723: `agg.num_cte` (a decompose road read through `Facts::road_on`, listed in `gravity_decompose::ROADS`), a
/// min / max of a CTE's one-scale numeric column over its order keys, off until its A/B and the user's call.
/// D3336 (A1a): `win.partfill` (face/partfill.rs, a parted window's fill as claims over K-cut parts), off until its A/B and
/// the user's call.
/// brfix: `mask.rows` (frame/mask_rows.rs, the first filter's mask made row by row over a source with no word window), off
/// until its A/B and the user's call.
/// D4821: `join.dedup` (ops/hashjoin/strategies/dedup.rs, copies of a build record dropped at the lay-out when the
/// probe's end keeps one copy of a row), off until its A/B and the user's call.
/// D4822: `text.substr.ascii` (rep/varlena/substr_ascii.rs, a hosted substring's result by bytes over ASCII text),
/// off until its A/B and the user's call.
/// D4794: `text.set.bpchar` (rep/varlena/text_set.rs, text.set's byte set over bpchar lists, trailing blanks trimmed on
/// both sides), off until its A/B and the user's call: text.set itself is on, so the widening waits for its own flip.
pub const DEFAULT_OFF: &[strategy::Strategy] = &[
    strategy::SCAN_MERGEFREE_BUILD,
    strategy::KEY_CARRY,
    strategy::GROUP_HASHCHAIN,
    strategy::SPOOL_SEAL,
    strategy::NUM_DIV,
    strategy::KEY_CANON,
    strategy::CTE_WHOLEFILL,
    strategy::NUM_CTE,
    strategy::WIN_PARTFILL,
    strategy::MASK_ROWS,
    strategy::JOIN_DEDUP,
    strategy::TEXT_SUBSTR_ASCII,
    strategy::TEXT_SET_BPCHAR,
];

/// Planned roads: declared, but no code path engages them (no mechanism, or a reader that returns before the road is
/// read), so none is an optimization on by default. Off by default as `DEFAULT_OFF` is (`default_off`); `+<id>` names one
/// on and changes nothing: a road with no registered row is never `on` (face/source.rs), and the three with one keep
/// their straight form either way. `srf.limitcut`'s only caller is the target-list ProjectSet, cut lazily before the road
/// is read (ops/srf/strategies/limit_cut.rs `rule`); `write.checks`' row is the host checks' admission, read through
/// `need_strategy` / `require` (no `ChecksAt::Ir` form exists). A planned road's mechanism lands with its own flip entry,
/// which takes it off this list.
pub const PLANNED: &[strategy::Strategy] = &[
    strategy::MERGE_MULT,
    strategy::MERGE_SKIP,
    strategy::NEST_BATCH,
    strategy::NEST_EAGERSAFE,
    strategy::NEST_GROUPJOIN,
    strategy::NEST_SELFSPOOL,
    strategy::SPOOL_ALIAS,
    strategy::WIN_FRAMECLASS,
    strategy::WIN_INVERSE,
    strategy::WIN_PREFIX,
    strategy::WIN_TOPN,
    strategy::JOIN_EMIT_BUILD,
    strategy::MARKS_TESTSET,
    strategy::NEST_BULK,
    strategy::NEST_CACHE,
    strategy::NEST_EQKEY,
    strategy::SEMIJOIN_MARKS,
    strategy::SEMIJOIN_RUNS,
    strategy::SETOP_COUNT,
    strategy::WIN_OWNEDVIEW,
    strategy::WIN_PARTTABLE,
    strategy::PREFETCH_TABLE,
    strategy::SRF_LIMITCUT,
    strategy::KEY_SURROGATE,
    strategy::WRITE_CHECKS,
];

/// Core roads: the only implementation of their operation, so no optional road. Their readers never read the
/// off set (`need_strategy`, the raw registry row, or a call by name), or off only refuses (`table.bypass`, the resolver's
/// last policy; `expr.monotone`, which admits MIN / MAX / ORDER BY f(x) at PostgreSQL's per-row form and has no other).
/// `gravity_road_off` naming one, plainly or `+<id>`, is the statement's ERROR 22023, as an unknown id is.
pub const CORE: &[strategy::Strategy] = &[
    strategy::TABLE_RADIX,
    strategy::DISTINCT_TOTAL,
    strategy::RUN_PERGROUP,
    strategy::NEST_CATALOG_LEFT,
    strategy::MERGE_MARKABLE,
    strategy::WRITE_LITERALS,
    strategy::WRITE_ROUTE,
    strategy::TABLE_BYPASS,
    strategy::EXPR_MONOTONE,
];

/// Fallback roads: the form a statement takes while its preferred road is off, never engaged by default, so no optional
/// road on by default either: `mask.word`, the scalar word mask, under `mask.vec` (step/filter.rs `block_road`). Its
/// switch still acts, so the option takes its name (m1_matrix's scalar row loop with every H01 road off).
pub const FALLBACK: &[strategy::Strategy] = &[strategy::MASK_WORD];

/// `DEFAULT_OFF` and `PLANNED` as a set: a backend's forced-off set before it reads the option (face/strategy.rs `OFF`),
/// and with no option table (unit tests, tools), so a default-off road stays off there too.
pub const fn default_off() -> strategy::StrategySet {
    let (mut w, mut i) = ([0u128; 2], 0);
    while i < DEFAULT_OFF.len() + PLANNED.len() {
        let b = if i < DEFAULT_OFF.len() { DEFAULT_OFF[i] } else { PLANNED[i - DEFAULT_OFF.len()] }.bit();
        w[b / 128] |= 1u128 << (b % 128);
        i += 1;
    }
    strategy::StrategySet(w)
}

/// `gravity_road_off` (plan §3.3, D255): the roads forced off for twin A/Bs and dev, `id,id,..`; REFCELLS VOIDs any run
/// with it set. A placeholder option, so `SET` takes it and the next statement sees it.
pub const ROAD_OFF_GUC: &str = "pgrust.gravity_road_off";

/// At the top of every lowering: the option read into `strategy::set_off`, then the log line.
pub(crate) fn apply() {
    read();
    log();
}

/// Reads `ROAD_OFF_GUC` into `strategy::set_off` at the top of every lowering; an unknown id or a `CORE` road's is the
/// statement's ERROR (22023) until the value is fixed.
/// With no option table (the seam uninstalled: unit tests, tools) the forced-off set stands as `strategy::set_off` left it.
pub(crate) fn read() {
    let Some(v) = crate::inputs::road_off() else { return };
    match parse(v.as_deref().unwrap_or("")) {
        Ok((named, on)) => {
            let mut set = named;
            DEFAULT_OFF.iter().chain(PLANNED).filter(|r| !on.has(**r)).for_each(|r| set.add(*r));
            NAMED_OFF.with(|c| c.set(named));
            strategy::set_off(set)
        }
        Err(id) => {
            let what = match strategy::Strategy::from_name(id.trim_start_matches('+').trim()) {
                Some(_) => "core road (no off switch)",
                None => "unknown road",
            };
            let msg = format!("invalid value for parameter \"{ROAD_OFF_GUC}\": {what} \"{id}\""); // alloc: Cold
            std::panic::panic_any(types_error::PgError::error(msg).with_sqlstate(types_error::error::ERRCODE_INVALID_PARAMETER_VALUE));
        }
    }
}

/// The option's value: the roads it names off, and the default-off roads it turns on (`+<id>`); an unknown id or a core
/// road's is the error.
fn parse(s: &str) -> Result<(strategy::StrategySet, strategy::StrategySet), &str> {
    let (mut off, mut on) = (strategy::StrategySet::EMPTY, strategy::StrategySet::EMPTY);
    let road = |id: &str| strategy::Strategy::from_name(id).filter(|r| !CORE.contains(r));
    for id in s.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        match id.strip_prefix('+') {
            Some(r) => on.add(road(r.trim()).ok_or(id)?),
            None => off.add(road(id).ok_or(id)?),
        }
    }
    Ok((off, on))
}

/// A lowering under forced-off roads (`gravity_road_off`, twin A/Bs and dev only) logs `gravity: roads off: <ids>` once per
/// backend per set, whoever set it: REFCELLS stamps VOID every cell whose server log shows the line (main 09-23: no cell,
/// served count or wrong-rows report is taken with a road forced off). A default-off road the option left alone is no
/// forced-off road: it is not named.
pub(crate) fn log() {
    let off = NAMED_OFF.with(core::cell::Cell::get);
    if !off.is_empty() && LOGGED_OFF.with(|c| c.replace(off)) != off {
        let ids: Vec<&str> = off.iter().map(strategy::Strategy::name).collect(); // alloc: Cold
        eprintln!("LOG: gravity: roads off: {}", ids.join(",")); // alloc: Cold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        static GUC: core::cell::Cell<Option<&'static str>> = const { core::cell::Cell::new(None) };
    }

    fn guc(name: &str) -> types_error::PgResult<Option<String>> {
        Ok(GUC.with(core::cell::Cell::get).filter(|_| name == ROAD_OFF_GUC).map(str::to_owned))
    }

    /// The option seam, once per process (the tests share it; each thread's value is its own `GUC`).
    fn install() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| guc_seams::get_config_option_missing_ok::set(guc));
    }

    /// D255: `SET pgrust.gravity_road_off = floor` admits a floor-applicable statement (floor applies to every one) without
    /// floor, `RESET` puts floor back, and an unknown id is the statement's ERROR 22023.
    #[test]
    fn road_off_is_the_set_able_guc() {
        install();
        let ctx = gravity_driver::mem::root::context("test", true);
        let p = crate::props::Props { need: mcx::PgVec::new_in(ctx.mcx()), kinds: crate::face::kinds::KindSet::EMPTY, props: crate::applies::PropSet::default(), max_rows: 0, pipelines: 1 };
        let with = |v: Option<&'static str>| {
            GUC.with(|g| g.set(v));
            read();
            crate::applies::applicable(&p).has(strategy::FLOOR)
        };
        assert!(with(None), "floor applies with the option never set");
        assert!(!with(Some("floor")), "SET gravity_road_off = floor takes floor off admission");
        assert!(with(Some("")), "RESET puts floor back");
        let e = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| with(Some("floor, no_such_road")))).expect_err("an unknown id errors");
        let e = e.downcast_ref::<types_error::PgError>().expect("a PgError");
        assert_eq!(e.sqlstate(), types_error::error::ERRCODE_INVALID_PARAMETER_VALUE);
    }

    /// D4497: every road decompose reads (`gravity_decompose::ROADS`, through `Facts::road_on`) is a strategy, so
    /// `SET pgrust.gravity_road_off` takes its name, and is default off: decompose reads it on only under `+<id>`, as
    /// hosts residency.rs `road_named_on` reads the option's text (a flip entry moves it to `ROADS_DEFAULT_ON`).
    #[test]
    fn decompose_roads_are_strategies() {
        for r in gravity_decompose::ROADS {
            let s = strategy::Strategy::from_name(r).unwrap_or_else(|| panic!("decompose reads road {r}, which no strategy declares"));
            assert!(DEFAULT_OFF.contains(&s), "decompose reads road {r} as default off (Facts::road_on), lowerv4 does not list it");
        }
    }

    /// D4565 / D4567: every road decompose reads through `Facts::default_on_road_on` (`gravity_decompose::ROADS_DEFAULT_ON`)
    /// is a strategy NOT on `DEFAULT_OFF`, read off only when the option names it plainly, as hosts residency.rs
    /// `road_named_off` reads the option's text, and none is in `ROADS` (`Facts::road_on`'s default-off roads). D4497 /
    /// D3227 K3's four `nest.keep.*` are on that list since their flip, and D3227 (b)'s `nest.key.eq` and D4620's
    /// `agg.gsets.cascade` since theirs.
    #[test]
    fn decompose_default_on_roads_are_default_on() {
        for r in gravity_decompose::ROADS_DEFAULT_ON {
            let s = strategy::Strategy::from_name(r).unwrap_or_else(|| panic!("decompose reads road {r}, which no strategy declares"));
            assert!(!DEFAULT_OFF.contains(&s), "decompose reads road {r} as on by default (Facts::default_on_road_on), lowerv4 lists it default off");
            assert!(!gravity_decompose::ROADS.contains(r), "road {r} on both decompose lists");
        }
    }

    /// D3194: `join.rowfilter` is on by default; `+<id>` on a road already on changes nothing, naming it plainly takes it off
    /// and logs it, and `+` with an unknown id errors as any unknown id does. D4585: `scan.mergefree` is on by default and
    /// off under its plain name; `scan.mergefree.build` is a road default off: off with the option unset, on under its own
    /// `+<id>` only, and a plain name keeps it off; D3324 R3's `key.carry` the same; D4676's `group.hashchain` the same.
    /// D3168's `join.probe.staged` is on since its flip entry, off under a plain name.
    /// D4567 (a)'s `agg.num_div` and D4723's `agg.num_cte` the same.
    /// D4569's `append.nest`, `join.rtfilter.arms` and `drain.spool.wide`, D3233's `case.select`, D3323's
    /// `join.rowfilter.single` and `join.rowfilter.cost`, and D4565's `agg.stat_reg` and D4567's `agg.num_scale` (since
    /// their flip entries) are on by default, off under a plain name; `join.rowfilter` named off leaves D3323's roads on.
    /// D3227 (b)'s `nest.key.eq`, textkeys' `text.lean` / `text.set` and D4620's `agg.gsets.cascade` the same since their
    /// flip entries.
    #[test]
    fn a_plus_id_turns_on_and_a_plain_id_turns_off() {
        install();
        let set = |v: Option<&'static str>| {
            GUC.with(|g| g.set(v));
            read();
            (strategy::off().has(strategy::JOIN_ROWFILTER), NAMED_OFF.with(core::cell::Cell::get).has(strategy::JOIN_ROWFILTER), strategy::off().has(strategy::FLOOR))
        };
        assert!(
            DEFAULT_OFF
                == [
                    strategy::SCAN_MERGEFREE_BUILD,
                    strategy::KEY_CARRY,
                    strategy::GROUP_HASHCHAIN,
                    strategy::SPOOL_SEAL,
                    strategy::NUM_DIV,
                    strategy::KEY_CANON,
                    strategy::CTE_WHOLEFILL,
                    strategy::NUM_CTE,
                    strategy::WIN_PARTFILL,
                    strategy::MASK_ROWS,
                    strategy::JOIN_DEDUP,
                    strategy::TEXT_SUBSTR_ASCII,
                    strategy::TEXT_SET_BPCHAR
                ],
            "the default-off roads (D4585, D3324 R3, D4497, D3227, D3227 (b), D4676, D4628, textkeys text.lean, D4620, textkeys text.set, D4567 (a), textkeys key.canon, D3336 A2 (iii), D4723, D3336 A1a, brfix, D4821 join.dedup, D4822 text.substr.ascii, D4794 text.set.bpchar; D4565 / D4567 / D3323 / D3168 flipped on)"
        );
        assert!(!default_off().has(strategy::SCAN_MERGEFREE) && default_off().has(strategy::SCAN_MERGEFREE_BUILD) && default_off().has(strategy::KEY_CARRY) && !default_off().has(strategy::CASE_SELECT) && !default_off().has(strategy::JOIN_ROWFILTER));
        assert!(
            [strategy::NEST_KEEP_INITPLAN, strategy::NEST_KEEP_SUBQUERY, strategy::NEST_KEEP_SEMI, strategy::NEST_KEEP_MATERIAL].iter().all(|r| !default_off().has(*r)),
            "D4497 / D3227's nest.keep roads on by default (decompose ROADS_DEFAULT_ON)"
        );
        assert!(
            [strategy::NEST_KEY_EQ, strategy::TEXT_LEAN, strategy::TEXT_SET, strategy::GSETS_CASCADE].iter().all(|r| !default_off().has(*r)),
            "D3227 (b)'s nest.key.eq, textkeys' text.lean / text.set and D4620's agg.gsets.cascade on by default since their flip"
        );
        assert!(!default_off().has(strategy::MERGE_WIDEEMIT) && !default_off().has(strategy::EMIT_PARMERGE), "D3336's roads on by default");
        let off = |v: Option<&'static str>| {
            GUC.with(|g| g.set(v));
            read();
            (strategy::off().has(strategy::SCAN_MERGEFREE), strategy::off().has(strategy::SCAN_MERGEFREE_BUILD))
        };
        assert_eq!(off(None), (false, true), "by default: the narrowed road on, its hash-build half off");
        assert_eq!(off(Some("+scan.mergefree")), (false, true), "+scan.mergefree on a road already on changes nothing");
        assert_eq!(off(Some("+scan.mergefree.build")), (false, false), "+scan.mergefree.build: S1 as it landed");
        assert_eq!(off(Some("scan.mergefree")), (true, true), "a plain name takes it off");
        assert_eq!(off(Some("scan.mergefree.build")), (false, true), "a plain name keeps the build half off");
        assert_eq!(off(Some("+join.rowfilter")), (false, true), "another road's +<id> leaves them at their defaults");
        let stat = |v: Option<&'static str>| {
            GUC.with(|g| g.set(v));
            read();
            strategy::off().has(strategy::STAT_REG)
        };
        assert!(!default_off().has(strategy::STAT_REG) && !stat(None), "agg.stat_reg on by default (D4565 flip)");
        assert!(!stat(Some("+agg.stat_reg")), "+agg.stat_reg on a road already on changes nothing");
        assert!(stat(Some("agg.stat_reg")) && stat(Some("+agg.stat_reg, agg.stat_reg")), "a plain name takes it off");
        let scale = |v: Option<&'static str>| {
            GUC.with(|g| g.set(v));
            read();
            strategy::off().has(strategy::NUM_SCALE)
        };
        assert!(!default_off().has(strategy::NUM_SCALE) && !scale(None), "agg.num_scale on by default (D4567 flip)");
        assert!(scale(Some("agg.num_scale")) && !scale(Some("agg.stat_reg")), "its plain name takes it off, another road's does not");
        let div = |v: Option<&'static str>| {
            GUC.with(|g| g.set(v));
            read();
            strategy::off().has(strategy::NUM_DIV)
        };
        assert!(default_off().has(strategy::NUM_DIV) && div(None), "agg.num_div off by default (D4567 (a))");
        assert!(!div(Some("+agg.num_div")) && div(Some("agg.num_div")) && div(Some("+agg.num_scale")), "on under +agg.num_div alone");
        let cte = |v: Option<&'static str>| {
            GUC.with(|g| g.set(v));
            read();
            strategy::off().has(strategy::NUM_CTE)
        };
        assert!(default_off().has(strategy::NUM_CTE) && cte(None), "agg.num_cte off by default (D4723)");
        assert!(!cte(Some("+agg.num_cte")) && cte(Some("+agg.num_div")), "on under +agg.num_cte alone");
        let carry = |v: Option<&'static str>| {
            GUC.with(|g| g.set(v));
            read();
            strategy::off().has(strategy::KEY_CARRY)
        };
        assert!(carry(None), "key.carry off by default (D3324 R3)");
        assert!(!carry(Some("+key.carry")), "on under +key.carry");
        assert!(carry(Some("key.carry")) && carry(Some("+scan.mergefree")), "a plain name or another road's +<id> keeps it off");
        let d3336 = |v: Option<&'static str>| {
            GUC.with(|g| g.set(v));
            read();
            (strategy::off().has(strategy::MERGE_WIDEEMIT), strategy::off().has(strategy::EMIT_PARMERGE))
        };
        assert_eq!(d3336(None), (false, false), "D3336's roads on by default");
        assert_eq!(d3336(Some("+merge.wideemit,+emit.parmerge")), (false, false), "both on by name, as by default");
        assert_eq!(d3336(Some("merge.wideemit")), (true, false), "each off by its own name");
        assert_eq!(d3336(Some("merge.wideemit,emit.parmerge")), (true, true), "both named off");
        let nest = |v: Option<&'static str>| {
            GUC.with(|g| g.set(v));
            read();
            (strategy::off().has(strategy::APPEND_NEST), NAMED_OFF.with(core::cell::Cell::get).has(strategy::APPEND_NEST))
        };
        assert_eq!(nest(None), (false, false), "append.nest on by default (D4569)");
        assert_eq!(nest(Some("+append.nest")), (false, false), "+append.nest on a road already on changes nothing");
        assert_eq!(nest(Some("append.nest")), (true, true), "a plain name takes it off, named");
        let arms = |v: Option<&'static str>| {
            GUC.with(|g| g.set(v));
            read();
            (strategy::off().has(strategy::JOIN_RTFILTER_ARMS), strategy::off().has(strategy::JOIN_RTFILTER), strategy::off().has(strategy::APPEND_NEST))
        };
        assert_eq!(arms(None), (false, false, false), "join.rtfilter.arms on by default, beside join.rtfilter and append.nest");
        assert_eq!(arms(Some("join.rtfilter.arms")), (true, false, false), "a plain name takes it off alone");
        assert_eq!(arms(Some("append.nest, join.rtfilter.arms")), (true, false, true), "both D4569 roads named off");
        let select = |v: Option<&'static str>| {
            GUC.with(|g| g.set(v));
            read();
            strategy::off().has(strategy::CASE_SELECT)
        };
        assert!(!select(None) && !select(Some("+key.carry")), "case.select on by default and under another road's +<id> (D3233)");
        assert!(!select(Some("+case.select")) && select(Some("case.select")), "+case.select changes nothing, a plain name takes it off");
        let wide = |v: Option<&'static str>| {
            GUC.with(|g| g.set(v));
            read();
            (strategy::off().has(strategy::DRAIN_SPOOL_WIDE), strategy::off().has(strategy::APPEND_NEST))
        };
        assert_eq!(wide(None), (false, false), "drain.spool.wide on by default, beside append.nest (D4569)");
        assert_eq!(wide(Some("+drain.spool.wide")), (false, false), "+drain.spool.wide on a road already on changes nothing");
        assert_eq!(wide(Some("drain.spool.wide")), (true, false), "a plain name takes it off alone");
        let rf = |v: Option<&'static str>| {
            GUC.with(|g| g.set(v));
            read();
            (strategy::off().has(strategy::JOIN_ROWFILTER_SINGLE), strategy::off().has(strategy::JOIN_ROWFILTER_COST), strategy::off().has(strategy::JOIN_ROWFILTER))
        };
        assert_eq!(rf(None), (false, false, false), "D3323's roads on by default since their flip, join.rowfilter on");
        assert_eq!(rf(Some("+join.rowfilter.single,+join.rowfilter.cost")), (false, false, false), "+<id> on a road already on changes nothing");
        assert_eq!(rf(Some("join.rowfilter.cost")), (false, true, false), "each off alone under its plain name");
        assert_eq!(rf(Some("join.rowfilter.single,join.rowfilter.cost")), (true, true, false), "both off");
        assert_eq!(rf(Some("join.rowfilter")), (false, false, true), "join.rowfilter named off; D3323's roads stay on");
        let staged = |v: Option<&'static str>| {
            GUC.with(|g| g.set(v));
            read();
            strategy::off().has(strategy::JOIN_PROBE_STAGED)
        };
        assert!(!default_off().has(strategy::JOIN_PROBE_STAGED) && !staged(None), "join.probe.staged on by default since its flip (D3168)");
        assert!(!staged(Some("+join.probe.staged")) && !staged(Some("join.rowfilter.cost")), "+<id> on a road already on changes nothing, another road's name leaves it on");
        assert!(staged(Some("join.probe.staged")) && staged(Some("+join.probe.staged, join.probe.staged")), "a plain name takes it off");
        let key = |v: Option<&'static str>| {
            GUC.with(|g| g.set(v));
            read();
            strategy::off().has(strategy::NEST_KEY_EQ)
        };
        assert!(!key(None), "nest.key.eq on by default (D3227 (b) flip)");
        assert!(!key(Some("+nest.key.eq")), "+<id> on a road already on changes nothing");
        assert!(key(Some("nest.key.eq")), "nest.key.eq off under its plain name");
        assert!(!key(Some("nest.keep.material")), "another nest road's plain name leaves it at its default");
        let chain = |v: Option<&'static str>| {
            GUC.with(|g| g.set(v));
            read();
            strategy::off().has(strategy::GROUP_HASHCHAIN)
        };
        assert!(default_off().has(strategy::GROUP_HASHCHAIN) && chain(None), "group.hashchain off by default (D4676)");
        assert!(!chain(Some("+group.hashchain")) && chain(Some("join.probe.staged")), "on under its +<id>, not another road's");
        assert!(chain(Some("+group.hashchain, group.hashchain")), "a plain name keeps it off");
        let lean = |v: Option<&'static str>| {
            GUC.with(|g| g.set(v));
            read();
            strategy::off().has(strategy::TEXT_LEAN)
        };
        assert!(!default_off().has(strategy::TEXT_LEAN) && !lean(None), "text.lean on by default (textkeys step 1 flip)");
        assert!(!lean(Some("+text.lean")) && !lean(Some("text.set")) && lean(Some("text.lean")), "off under its plain name only");
        let tset = |v: Option<&'static str>| {
            GUC.with(|g| g.set(v));
            read();
            strategy::off().has(strategy::TEXT_SET)
        };
        assert!(!default_off().has(strategy::TEXT_SET) && !tset(None), "text.set on by default (textkeys step 2 flip)");
        assert!(!tset(Some("+text.set")) && !tset(Some("text.lean")) && tset(Some("text.set")), "off under its plain name only");
        let cascade = |v: Option<&'static str>| {
            GUC.with(|g| g.set(v));
            read();
            strategy::off().has(strategy::GSETS_CASCADE)
        };
        assert!(!default_off().has(strategy::GSETS_CASCADE) && !cascade(None), "agg.gsets.cascade on by default (D4620 flip)");
        assert!(!cascade(Some("+agg.gsets.cascade")) && !cascade(Some("agg.num_div")) && cascade(Some("agg.gsets.cascade")), "off under its plain name only");
        let canon = |v: Option<&'static str>| {
            GUC.with(|g| g.set(v));
            read();
            strategy::off().has(strategy::KEY_CANON)
        };
        assert!(default_off().has(strategy::KEY_CANON) && canon(None), "key.canon off by default (textkeys step 3)");
        assert!(!canon(Some("+key.canon")) && canon(Some("+text.set")) && canon(Some("key.canon")), "on under its +<id> only");
        let rows = |v: Option<&'static str>| {
            GUC.with(|g| g.set(v));
            read();
            strategy::off().has(strategy::MASK_ROWS)
        };
        assert!(default_off().has(strategy::MASK_ROWS) && rows(None), "mask.rows off by default (brfix)");
        assert!(!rows(Some("+mask.rows")) && rows(Some("+join.probe.staged")), "on under its +<id>, not another road's");
        assert_eq!(set(None), (false, false, false), "on by default");
        assert_eq!(set(Some("+join.rowfilter")), (false, false, false), "on, as before");
        assert_eq!(set(Some("floor, +join.rowfilter")), (false, false, true), "on beside a forced-off road");
        assert_eq!(set(Some("join.rowfilter")), (true, true, false), "named off");
        assert!(std::panic::catch_unwind(|| set(Some("+no_such_road"))).is_err(), "an unknown id errors");
    }

    /// D4628: `spool.seal` is off by default, on under its own `+<id>` only, and a plain name keeps it off.
    #[test]
    fn spool_seal_is_off_until_named_on() {
        install();
        let off = |v: Option<&'static str>| {
            GUC.with(|g| g.set(v));
            read();
            strategy::off().has(strategy::SPOOL_SEAL)
        };
        assert!(default_off().has(strategy::SPOOL_SEAL) && off(None), "spool.seal off by default (D4628)");
        assert!(!off(Some("+spool.seal")) && off(Some("+join.probe.staged")), "on under its +<id>, not another road's");
        assert!(off(Some("+spool.seal, spool.seal")), "a plain name keeps it off");
    }

    /// D3336 (A2 (iii)): `cte.wholefill` is off by default, on under its own `+<id>` only, and a plain name keeps it off.
    #[test]
    fn cte_wholefill_is_off_until_named_on() {
        install();
        let off = |v: Option<&'static str>| {
            GUC.with(|g| g.set(v));
            read();
            strategy::off().has(strategy::CTE_WHOLEFILL)
        };
        assert!(default_off().has(strategy::CTE_WHOLEFILL) && off(None), "cte.wholefill off by default (D3336 A2 (iii))");
        assert!(!off(Some("+cte.wholefill")) && off(Some("+spool.seal")), "on under its +<id>, not another road's");
        assert!(off(Some("+cte.wholefill, cte.wholefill")), "a plain name keeps it off");
    }

    /// D3336 (A1a): `win.partfill` is off by default, on under its own `+<id>` only, and a plain name keeps it off.
    #[test]
    fn win_partfill_is_off_until_named_on() {
        install();
        let off = |v: Option<&'static str>| {
            GUC.with(|g| g.set(v));
            read();
            strategy::off().has(strategy::WIN_PARTFILL)
        };
        assert!(default_off().has(strategy::WIN_PARTFILL) && off(None), "win.partfill off by default (D3336 A1a)");
        assert!(!off(Some("+win.partfill")) && off(Some("+spool.seal")), "on under its +<id>, not another road's");
        assert!(off(Some("+win.partfill, win.partfill")), "a plain name keeps it off");
        assert!(!default_off().has(strategy::WIN_PARTRUNS), "win.partruns stays on by default");
    }

    /// D4821: `join.dedup` is off by default, on under its own `+<id>` only, and a plain name keeps it off.
    #[test]
    fn join_dedup_is_off_until_named_on() {
        install();
        let off = |v: Option<&'static str>| {
            GUC.with(|g| g.set(v));
            read();
            strategy::off().has(strategy::JOIN_DEDUP)
        };
        assert!(default_off().has(strategy::JOIN_DEDUP) && off(None), "join.dedup off by default (D4821)");
        assert!(!off(Some("+join.dedup")) && off(Some("+join.probe.staged")), "on under its +<id>, not another road's");
        assert!(off(Some("+join.dedup, join.dedup")), "a plain name keeps it off");
        assert!(!default_off().has(strategy::JOIN_MULT), "join.mult stays on by default");
    }

    /// D4822: `text.substr.ascii` is off by default and on under its own `+<id>` only.
    #[test]
    fn text_substr_ascii_is_off_until_named_on() {
        install();
        let off = |v: Option<&'static str>| {
            GUC.with(|g| g.set(v));
            read();
            strategy::off().has(strategy::TEXT_SUBSTR_ASCII)
        };
        assert!(default_off().has(strategy::TEXT_SUBSTR_ASCII) && off(None), "text.substr.ascii off by default (D4822)");
        assert!(!off(Some("+text.substr.ascii")) && off(Some("+join.dedup")), "on under its +<id>, not another road's");
    }

    /// D4794: `text.set.bpchar` is off by default and on under its own `+<id>` only, while text.set stays on.
    #[test]
    fn text_set_bpchar_is_off_until_named_on() {
        install();
        let off = |v: Option<&'static str>| {
            GUC.with(|g| g.set(v));
            read();
            strategy::off().has(strategy::TEXT_SET_BPCHAR)
        };
        assert!(default_off().has(strategy::TEXT_SET_BPCHAR) && off(None), "text.set.bpchar off by default (D4794)");
        assert!(!off(Some("+text.set.bpchar")) && off(Some("+text.substr.ascii")), "on under its +<id>, not another road's");
        assert!(!default_off().has(strategy::TEXT_SET), "text.set stays on by default");
    }

    /// genreg P6 (§10 item 7): `join.payload_fp` and `join.drain.fp` (ops/hashjoin/strategies/payload_fp.rs) are on by
    /// default, each taken off alone by its plain name (they had no off switch: decided in the build's cost guard only).
    #[test]
    fn join_payload_fp_and_drain_fp_are_on_until_named_off() {
        install();
        let off = |v: Option<&'static str>| {
            GUC.with(|g| g.set(v));
            read();
            (strategy::off().has(strategy::JOIN_PAYLOAD_FP), strategy::off().has(strategy::JOIN_DRAIN_FP))
        };
        assert!(!default_off().has(strategy::JOIN_PAYLOAD_FP) && !default_off().has(strategy::JOIN_DRAIN_FP), "both on by default");
        assert_eq!(off(None), (false, false), "both on with no option");
        assert_eq!(off(Some("+join.payload_fp")), (false, false), "+<id> on a road already on changes nothing");
        assert_eq!(off(Some("join.payload_fp")), (true, false), "a plain name takes join.payload_fp off alone");
        assert_eq!(off(Some("join.drain.fp")), (false, true), "a plain name takes join.drain.fp off alone");
        assert_eq!(off(Some("join.payload_fp, join.drain.fp")), (true, true), "both named off");
    }

    /// The planned roads are off by default and `+<id>` names them on with no error; a core road takes no name in the
    /// option, plainly or `+<id>` (ERROR 22023, as an unknown id); the fallback `mask.word` keeps its switch;
    /// `distinct.identity` is deleted. Names only, so the test runs unchanged on trunk (its born-red witness).
    #[test]
    fn planned_roads_are_off_core_roads_take_no_switch_and_distinct_identity_is_gone() {
        install();
        const PLANNED_IDS: &[&str] = &[
            "merge.mult", "merge.skip", "nest.batch", "nest.eagersafe", "nest.groupjoin", "nest.selfspool", "spool.alias",
            "win.frameclass", "win.inverse", "win.prefix", "win.topn", "join.emit_build", "marks.testset", "nest.bulk",
            "nest.cache", "nest.eqkey", "semijoin.marks", "semijoin.runs", "setop.count", "win.ownedview", "win.parttable",
            "prefetch.table", "srf.limitcut", "key.surrogate", "write.checks",
        ];
        const CORE_IDS: &[&str] =
            &["table.radix", "distinct.total", "run.pergroup", "nest.catalog_left", "merge.markable", "write.literals", "write.route", "table.bypass",
              "expr.monotone"];
        let road = |id: &str| strategy::Strategy::from_name(id).unwrap_or_else(|| panic!("road {id} is declared"));
        let set = |v: &'static str| {
            GUC.with(|g| g.set(Some(v)));
            std::panic::catch_unwind(read)
                .map(|()| strategy::off())
                .map_err(|e| e.downcast_ref::<types_error::PgError>().map(|e| e.sqlstate() == types_error::error::ERRCODE_INVALID_PARAMETER_VALUE))
        };
        let off = set("").expect("the option empty");
        for &id in PLANNED_IDS {
            assert!(default_off().has(road(id)) && off.has(road(id)), "planned road {id} is off by default");
        }
        let named_on: &'static str = Box::leak(PLANNED_IDS.iter().map(|id| format!("+{id}")).collect::<Vec<_>>().join(",").into_boxed_str());
        let on = set(named_on).expect("+<id> names a planned road on");
        assert!(PLANNED_IDS.iter().all(|&id| !on.has(road(id))), "+<id> names every planned road on");
        for &id in CORE_IDS {
            assert!(!default_off().has(road(id)), "core road {id} is on");
            let plus: &'static str = Box::leak(format!("+{id}").into_boxed_str());
            for v in [id, plus] {
                assert_eq!(set(v).err(), Some(Some(true)), "gravity_road_off = {v}: a core road has no switch (ERROR 22023)");
            }
        }
        assert!(set("mask.word").is_ok_and(|o| o.has(road("mask.word"))), "the fallback mask.word keeps its switch");
        assert!(strategy::Strategy::from_name("distinct.identity").is_none(), "distinct.identity is deleted");
        assert_eq!(set("distinct.identity").err(), Some(Some(true)), "its name is an unknown id");
        set("").expect("the option empty");
    }

    /// `join.sorted` and `nest.key.wide` are deleted: no strategy declares either name, so naming one in
    /// `pgrust.gravity_road_off` is the unknown id's ERROR 22023, and `nest.key.eq` keeps its own switch. Names only, so
    /// the test runs unchanged on the tree before the deletion (its born-red witness).
    #[test]
    fn join_sorted_and_nest_key_wide_are_gone() {
        install();
        let set = |v: &'static str| {
            GUC.with(|g| g.set(Some(v)));
            std::panic::catch_unwind(read)
                .map(|()| strategy::off())
                .map_err(|e| e.downcast_ref::<types_error::PgError>().map(|e| e.sqlstate() == types_error::error::ERRCODE_INVALID_PARAMETER_VALUE))
        };
        for id in ["join.sorted", "nest.key.wide"] {
            assert!(strategy::Strategy::from_name(id).is_none(), "{id} is deleted");
            let plus: &'static str = Box::leak(format!("+{id}").into_boxed_str());
            for v in [id, plus] {
                assert_eq!(set(v).err(), Some(Some(true)), "gravity_road_off = {v}: an unknown id (ERROR 22023)");
            }
        }
        assert!(!gravity_decompose::ROADS_DEFAULT_ON.contains(&"nest.key.wide"), "decompose reads no nest.key.wide");
        assert!(set("nest.key.eq").expect("nest.key.eq keeps its switch").has(strategy::NEST_KEY_EQ), "nest.key.eq off under its plain name");
        set("").expect("the option empty");
    }

    /// `code.ties` is deleted: no strategy declares the name, so naming it in `pgrust.gravity_road_off` is the unknown id's
    /// ERROR 22023, and `code.cut` keeps its switch. Names only, so the test runs unchanged on the tree before the deletion
    /// (its born-red witness).
    #[test]
    fn code_ties_is_gone() {
        install();
        let set = |v: &'static str| {
            GUC.with(|g| g.set(Some(v)));
            std::panic::catch_unwind(read)
                .map(|()| strategy::off())
                .map_err(|e| e.downcast_ref::<types_error::PgError>().map(|e| e.sqlstate() == types_error::error::ERRCODE_INVALID_PARAMETER_VALUE))
        };
        assert!(strategy::Strategy::from_name("code.ties").is_none(), "code.ties is deleted");
        for v in ["code.ties", "+code.ties"] {
            assert_eq!(set(v).err(), Some(Some(true)), "gravity_road_off = {v}: an unknown id (ERROR 22023)");
        }
        assert!(set("code.cut").expect("code.cut keeps its switch").has(strategy::CODE_CUT), "code.cut off under its plain name");
        set("").expect("the option empty");
    }

    /// `twopass.marks` is deleted: no strategy declares the name, so naming it in `pgrust.gravity_road_off` is the unknown
    /// id's ERROR 22023, and `twopass.having` keeps its switch. Names only, so the test runs unchanged on the tree before
    /// the deletion (its born-red witness).
    #[test]
    fn twopass_marks_is_gone() {
        install();
        let set = |v: &'static str| {
            GUC.with(|g| g.set(Some(v)));
            std::panic::catch_unwind(read)
                .map(|()| strategy::off())
                .map_err(|e| e.downcast_ref::<types_error::PgError>().map(|e| e.sqlstate() == types_error::error::ERRCODE_INVALID_PARAMETER_VALUE))
        };
        assert!(strategy::Strategy::from_name("twopass.marks").is_none(), "twopass.marks is deleted");
        for v in ["twopass.marks", "+twopass.marks"] {
            assert_eq!(set(v).err(), Some(Some(true)), "gravity_road_off = {v}: an unknown id (ERROR 22023)");
        }
        assert!(set("twopass.having").expect("twopass.having keeps its switch").has(strategy::TWOPASS_HAVING), "twopass.having off under its plain name");
        set("").expect("the option empty");
    }

    /// `run.measure` is deleted (D4941): no strategy declares the name, so naming it in `pgrust.gravity_road_off` is the
    /// unknown id's ERROR 22023; `num.partial` keeps its off name, and bit 126 stays free.
    #[test]
    fn run_measure_is_gone() {
        install();
        let set = |v: &'static str| {
            GUC.with(|g| g.set(Some(v)));
            std::panic::catch_unwind(read)
                .map(|()| strategy::off())
                .map_err(|e| e.downcast_ref::<types_error::PgError>().map(|e| e.sqlstate() == types_error::error::ERRCODE_INVALID_PARAMETER_VALUE))
        };
        assert!(strategy::Strategy::from_name("run.measure").is_none(), "run.measure is deleted");
        for v in ["run.measure", "+run.measure"] {
            assert_eq!(set(v).err(), Some(Some(true)), "gravity_road_off = {v}: an unknown id (ERROR 22023)");
        }
        assert!(set("num.partial").expect("num.partial keeps its off name").has(strategy::NUM_PARTIAL), "num.partial off under its name");
        assert!(strategy::Strategy::all().all(|r| r.bit() != 126), "bit 126 stays free");
        set("").expect("the option empty");
    }

    /// The road classes stay apart (no road in two of `DEFAULT_OFF`, `PLANNED`, `CORE` and `FALLBACK`); a planned road
    /// has no registered row but `srf.limitcut` and `write.checks` (their rows are the straight form's admission), and every
    /// core and fallback road has one.
    #[test]
    fn road_classes_are_disjoint_and_planned_roads_have_no_mechanism() {
        let lists = [DEFAULT_OFF, PLANNED, CORE, FALLBACK];
        for (i, a) in lists.iter().enumerate() {
            for r in a.iter() {
                assert!(lists[i + 1..].iter().all(|b| !b.contains(r)), "{} in two road classes", r.name());
            }
        }
        let built = crate::ops::built_strategies();
        for r in PLANNED {
            assert_eq!(built.has(*r), [strategy::SRF_LIMITCUT, strategy::WRITE_CHECKS].contains(r), "planned road {} registered", r.name());
        }
        assert!(CORE.iter().chain(FALLBACK).all(|r| built.has(*r)), "a core or fallback road with no registered row");
    }
}

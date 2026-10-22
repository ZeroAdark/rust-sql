//! Sized from bound / observation, grown in place, lazy zero. Hooks / levers: H13b R3-L02. Budget 160 lines (files.tsv,
//! +25 % gate). Owner M1-TABLES.
//!
//! A keyed table is sized to its bound's working set, never to the planner's estimate (R3-L02): the `TableSize` bind fact
//! is the group bound the runner derives from exact facts (the surviving rows; the key domains' product when every domain
//! is known) or remembers from a finished run under the plan key, 0 when there is none. The table's open reads it from the
//! table's Facts words (`TableDecl.facts`), which the maker declares here once per table.
//!
//! An underestimate (a stale observation, stale statistics) never errors or loses a row: past its size the table evicts
//! into its partitions, observes, bypasses and flushes as any full table does, and the owner phase grows its region past
//! the share (D254). The grow in place (trunk's `T1::at_sized` / `T1::grow`, HOSTS' open and locate) is the speed half.
//!
//! The open of a plain Fp128 pair offers its layout to the driving site's source (host_abi v4 m1_tables
//! `entry_runs_offer`): road dict.entries' runs (hosts granule_source/entry_runs.rs).
//!
//! Registers: road `table.sized` (`TABLE_SIZED`), a marker: the open reads the bound (registered for
//! the sized open; the grow in place lands before any keyed or join bar cell of record).
use gravity_abi::breaker::{RecordLayout, t1_default_bytes};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_kernels::host_abi as abi;
use gravity_program::protos::{Fact, Proto, ProtoKind};

use crate::table::bypass::{fail_on, host, state, sync_bypass, Hashed, ST_APP, ST_LAYOUT, ST_T1, T1_LAYOUT};
use crate::table::face::{Domain, TableDecl, TablePolicy};
use crate::frame::env::Env;
use crate::frame::px::{Px, DRAFT_SITE};

crate::strategies! {
    /// Sized from bound / observation, grown in place, lazy zero.
    TABLE_SIZED = 35, "table.sized", Table;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(TABLE_SIZED, &())], ..crate::ops::NONE };

/// Table `t`'s Facts words on pipeline `k` (its `facts`): the dense domain's `DenseLo`, `DenseHi` of key lane `lane` when
/// the domain is bound at run time, then `TableSize`, in `Fact` order. Never word 0, so 0 stays "none". With the table's
/// breaker slot (`k` produces it), an `observe` on `k` remembers its finished groups for the next execution's `TableSize`.
pub(crate) fn declare_facts(px: &mut Px<'_, '_>, k: usize, t: &TableDecl, lane: u8) -> u16 {
    let dense = matches!(t.key.domain, Domain::Bound | Domain::Div { .. } | Domain::Shift { .. });
    let n = if dense { 3 } else { 1 };
    let mut base = px.proto_words(n);
    if base == 0 {
        base = px.proto_words(n);
    }
    let mut arg = 1u64 << Fact::TableSize as u8;
    let mut p = Proto::new(ProtoKind::Facts, k as u16).words(base, n);
    if dense {
        arg |= 1 << Fact::DenseLo as u8 | 1 << Fact::DenseHi as u8;
        p = p.site(DRAFT_SITE).slot(lane);
    }
    px.declare_proto(p.arg(arg));
    if let Some(br) = (t.life.0 as usize == k).then(|| crate::frame::iface::end_breaker_of(px.gx, k)).flatten() {
        px.declare_proto(Proto::new(ProtoKind::Observe, k as u16).slot(br.slot));
    }
    base
}

/// Word `i` of `t`'s Facts words.
fn fact(b: &mut FunctionBuilder<'_>, env: &Env, t: &TableDecl, i: u16) -> Value {
    b.load(Type::I64, env.params, 8 * (t.facts + i) as i32)
}

/// The group bound the table opens at (`agg_open`'s groups): `TableSize`, else the decl's compile-time bound. With neither,
/// a full phase-1 table's slots (`T1_DEFAULT_BYTES / record`): no bound never means a bypass at the open (`agg_open`'s
/// no-estimate rule), the evictions decide (`observe`).
pub(crate) fn groups(b: &mut FunctionBuilder<'_>, env: &Env, t: &TableDecl) -> Value {
    // D2988: a rerun table's planner numGroups (`TableDecl::expect`, below the full table only) stands for a missing fact
    let full = (t1_default_bytes() / t.layout.size()) as u64;
    let full = b.iconst(Type::I64, t.expect.filter(|&g| g < full).unwrap_or(full) as i64);
    let bound = match t.facts {
        0 => b.iconst(Type::I64, t.expect.unwrap_or(0) as i64),
        _ => fact(b, env, t, if matches!(t.key.domain, Domain::Bound | Domain::Div { .. } | Domain::Shift { .. }) { 2 } else { 0 }),
    };
    let none = b.icmp(IntCond::Eq, bound, env.zero);
    b.select(none, full, bound)
}

/// The run-time dense domain `(lo, hi)` (`DenseLo`, `DenseHi`; `(1, 0)` = none) of a `Domain::Bound` table; a `Domain::Div`
/// key `x / c` divides `x`'s lane bound by `c > 0` (truncating toward zero, as PostgreSQL: non-decreasing), a
/// `Domain::Shift` key `x + c` adds `c` to it (increasing; a bound that leaves i64 wraps below the other, so the domain is
/// none: a row there raises out of range in PostgreSQL anyway), none kept.
pub(crate) fn domain(b: &mut FunctionBuilder<'_>, env: &Env, t: &TableDecl) -> (Value, Value) {
    debug_assert!(t.facts != 0 && matches!(t.key.domain, Domain::Bound | Domain::Div { .. } | Domain::Shift { .. }), "a bound domain's table declared its facts");
    let (lo, hi) = (fact(b, env, t, 0), fact(b, env, t, 1));
    let (op, c) = match t.key.domain {
        Domain::Div { c } => (IntBinOp::Sdiv, c),
        Domain::Shift { c } => (IntBinOp::Add, c),
        _ => return (lo, hi),
    };
    let (none, c) = (b.icmp(IntCond::Slt, hi, lo), b.iconst(Type::I64, c));
    let (l, h) = (b.int_bin(op, lo, c), b.int_bin(op, hi, c));
    (b.select(none, env.one, l), b.select(none, env.zero, h))
}

/// The widest group count an open takes (D370): below `agg_open`'s flag words (`AGG_GROUPS_BYPASS` 1 << 61,
/// `AGG_GROUPS_OBSERVE` 1 << 62) and `t1_bytes`' power-of-two range, past any T1's slots (its `cap` is 32 bits), so no
/// sizing or estimate-bypass decision moves under it.
const GROUPS_MAX: i64 = u32::MAX as i64;

/// `layout`'s words in a stack slot (`agg_open` copies them into T1; the state then points at T1's copy).
fn layout_slot(b: &mut FunctionBuilder<'_>, layout: &RecordLayout) -> Value {
    let w: [u64; 5] = layout.words();
    let slot = b.declare_stack_slot(40, 8);
    let p = b.stack_addr(slot);
    for (i, w) in w.iter().enumerate() {
        let v = b.const_raw(Type::I64, *w);
        b.store(p, 8 * i as i32, v);
    }
    p
}

impl Hashed {
    /// The claim's open: T1 laid out or resumed at the radix bits and `groups` (`agg_open`; with table.sized on,
    /// `agg_open_grown`: opened at the bound's size with the default reserved, grown in place on a full home pair by the
    /// miss, `agg_locate`), the state's layout pointed at T1's own copy (the stack slot dies with the call), the append
    /// count cleared, the bypass word read. `groups` is taken within the table's bound (`groups`: a dense domain's span
    /// past it sizes at the bound) and below `GROUPS_MAX`: a count never reaches `agg_open`'s flag words (D370: a bigint
    /// key's span, 2^64 - 1.1e13 as an unsigned difference, set AGG_GROUPS_OBSERVE and _BYPASS and held its T1).
    pub(crate) fn open_with(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, groups: Value) {
        let (ctx, zero, x) = (env.ctx, env.zero, env.x);
        let (bound, max) = (self::groups(b, env, t), b.iconst(Type::I64, GROUPS_MAX));
        let groups = b.int_bin(IntBinOp::Umin, groups, bound);
        let groups = b.int_bin(IntBinOp::Umin, groups, max);
        let st = state(b, env, t);
        let lp = layout_slot(b, &t.layout);
        let bits = crate::table::radix::bits(b, env, t);
        let (name, f) = match TablePolicy::Sized.on() {
            true => ("agg_open_grown", gravity_abi::host_words!(gravity_kernels::table::entries::agg_open_grown(ctx, st, layout, bits, groups))),
            false => ("agg_open", gravity_abi::host_words!(abi::agg_open(ctx, st, layout, bits, groups))),
        };
        let open = host(b, name, &[Type::Ptr, Type::Ptr, Type::Ptr, Type::I64, Type::I64], f);
        let r = b.call_host(open, &[ctx, st, lp, bits, groups], Some(Type::I64)).expect("agg_open returns a status");
        fail_on(b, env, &x, r);
        if t.copies != 0 {
            // D3312: the slots holding key copies named on the table, for an in-memory compact's reclaim
            let f = gravity_abi::host_words!(abi::agg_copies(st, mask));
            let h = host(b, "agg_copies", &[Type::Ptr, Type::I64], f);
            let mask = b.const_raw(Type::I64, t.copies);
            b.call_host(h, &[st, mask], Some(Type::I64));
        }
        // road dict.entries' runs (hosts granule_source/entry_runs.rs): the table offers its layout to the driving
        // site's source, whose one premise (entry_runs.rs `accepts`: an identity fold of one entry image, the source's
        // recipe) decides; it then hands whole parts to the owner phase as resident runs, and a rejected offer keeps the
        // rows (KEYED-20: no second spelling of the layout here)
        if let Some(site) = env.site {
            let addr = gravity_abi::host_words!(gravity_kernels::dict::entries_tables::entry_runs_offer(ctx, site, layout, bits));
            let offer = b.declare_host_once("entry_runs_offer", &[Type::Ptr, Type::I64, Type::Ptr, Type::I64], Some(Type::I64), addr, HostFlags { touches_memory: true, ..HostFlags::default() });
            let s = b.iconst(Type::I64, site as i64);
            b.call_host(offer, &[ctx, s, lp, bits], Some(Type::I64));
        }
        let t1 = b.load(Type::Ptr, st, ST_T1);
        let tl = b.ptr_add(t1, zero, 1, T1_LAYOUT);
        b.store(st, ST_LAYOUT, tl);
        b.store(st, ST_APP, zero);
        sync_bypass(b, st);
    }
}

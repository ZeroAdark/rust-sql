//! D1904: a rerun set's INNER / SEMI probe over a table kept across the set's re-runs (built outside the set), whose outer
//! side raises or calls in the probe pipeline's own ops before the probe or in its outer hash keys. PG 18.6 builds the
//! table at the first call that runs its Hash (HJ_BUILD_HASHTABLE: build-first, or outer-first once the outer's first row
//! came), where an empty table ends the join before the rest of the outer; every later call reuses the one-batch table
//! (ExecReScanHashJoin) and goes to HJ_NEED_NEW_OUTER: the whole outer is scanned and every row's outer hash keys computed
//! (ExecHashJoinOuterGetTuple), an empty table too. The set runs every call as the first (D276's mask is the call's), so
//! the probe keeps two frame words: `seen`, never reset at an open (zero at the execution's binding, kept across the
//! set's calls), set once a call ran PG's build (build-first: at the call's first window; outer-first: when a row
//! reaches the probe); and `mode`, an open word, the call's own reading of `seen` at its first window. A later call
//! (`LATER`) never ends at an empty table: each row runs its ops and keys (a NULL key misses, D1898) and misses.
//! D1902 (`Order`): a member probe over a member build on the cost path (INNER / SEMI outer-first): HJ_BUILD_HASHTABLE
//! fetches the outer's first row before it builds only while hj_OuterNotEmpty is clear, and ExecReScanHashJoin keeps that
//! flag over a rebuild: a call after one whose outer gave a row with non-NULL keys builds first, and its empty table never
//! runs the outer. The set builds every call before the probe; a quiet build shows only whether it is empty, so the probe
//! keeps hj_OuterNotEmpty in a frame word and decides per call where an empty table ends the call.
use gravity_decompose::join::JoinDesc;
use gravity_decompose::{Op, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, IntBinOp, IntCond, Type, Value};
use types_nodes::jointype::JoinType;

use crate::face::kinds::Face;
use crate::frame::env::Env;
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{Px, WordKey};
use crate::graph::Gx;
use crate::ops::hashjoin::build_class::{defers, feed, quiet_statement, side_by, Side};

/// `mode`'s value in a call that runs PG's build (or no build yet), and in a later call over the kept table.
const FIRST: i64 = 1;
const LATER: i64 = 2;
/// `Order`'s `mode` in an outer-first call and in a build-first one.
const OUTER: i64 = 1;
const BUILD: i64 = 2;

/// The probe's two frame words (byte offsets) and its path.
pub(crate) struct Calls {
    mode: i32,
    seen: i32,
    outer_first: bool,
    /// join#212/213: a RIGHT SEMI / RIGHT ANTI probe marks the kept table's records: each call clears them first
    marks: bool,
    /// D1016 row 2 (`seek_keyed`): a build-first probe of a walk that evaluates its keys at its seek closes PG's build call
    /// over an EMPTY table at the open (`prologue`, `closes`); `shut` that call's verdict, computed at the prologue.
    close: bool,
    shut: core::cell::Cell<Option<Value>>,
}

/// join#212 / join#213: a RIGHT SEMI or RIGHT ANTI probe too; it marks the kept table's records, so each call clears them
/// at its first window (`Calls::at_window`), as PG's rescan resets the match flags of a table it keeps.
/// D1904 (served): probe `j` of pipeline `q` over build `b` takes `Calls`: an INNER or SEMI probe (not psql's catalog
/// form) of a rerun set's member over a build in no rerun set, whose outer side (`side_by` with its keys) is not quiet
/// while its source and feeders are (`side_by` through no op: they run at every call, the build-first call's too; an
/// index or bitmap outer over initdb opclasses reads as quiet, D2063's facts), and whose build side neither calls nor
/// hides a call (D1906's: PG rebuilds a multi-batch table) nor holds its error for the first row (op 14's deferral), in a
/// statement that holds no user code no expression shows, this probe's own hash clauses read by their operators (D3374,
/// visible/hashjoin.rs `quiet_statement_at`: a user or VOLATILE call in an outer key is the outer side served here, and
/// the source and feeders are read under it too, build_class.rs `side_by_at`: a LATERAL outer's index walk over initdb
/// opclasses reads as quiet beside such a key;
/// build_class.rs `quiet_statement` elsewhere: a user opclass's index, a
/// hash comparison through a user type, narrowed from any value of a user type or row-reading xml
/// builtin, which each expression's own reading now carries; the m8_d1902_quiet_sides C5 case, a table_to_xml one-time
/// filter above the join, serves with PG's calls, rig W0 and W8).
pub(crate) fn rescans(gx: &Gx<'_, '_>, q: usize, j: usize, b: usize) -> bool {
    let Some(Op::Join(JoinDesc::Probe { jointype, outer_first, catalog: false, .. })) = gx.pipelines()[q].ops.get(j) else { return false };
    let sets = || gx.graph.reruns.iter();
    matches!(jointype, JoinType::JOIN_INNER | JoinType::JOIN_SEMI | JoinType::JOIN_UNIQUE_OUTER | JoinType::JOIN_UNIQUE_INNER | JoinType::JOIN_RIGHT_SEMI | JoinType::JOIN_RIGHT_ANTI)
        && sets().any(|r| r.pipelines.contains(&(q as u16)))
        && !sets().any(|r| r.pipelines.contains(&(b as u16)))
        && side_by(gx, q, j, true) != Side::Safe
        && crate::ops::hashjoin::build_class::side_by_at(gx, q, 0, false, q, j) == Side::Safe
        && side_by(gx, b, usize::MAX, false) != Side::UserFn
        && !crate::ops::hashjoin::probe::opaque(gx, b)
        && !(*outer_first && defers(gx, b, q, j))
        && crate::effects::visible::hashjoin::quiet_statement_at(gx, q, j)
}

/// D3378 C3 / C4 / C5 (D1910 / D1762): probe `j` of nest inner `k` (an embedded inner: rerun/inner_probe.rs `ok`'s kept
/// road) takes `Calls` too: an INNER or SEMI probe (not psql's catalog form), `k`, the build `b` and its slot in no
/// rerun set, over a quiet build side (nothing `opaque`, no held error) whose outer side may raise (its hash keys, a
/// filter, gate or projection before it, a VALUES cell of its source; a PARAM_EXEC the nest binds read as bound), in a
/// statement whose user code every expression shows (`quiet_statement`). The table is built once, ahead of the owner,
/// as PG's one-batch table no changed parameter reads: ExecReScanHashJoin keeps it and goes to HJ_NEED_NEW_OUTER. PG
/// builds it at the first re-open (build-first: `Calls::at_window`, whose hook runs at every re-open the gate lets
/// through, before the source's first window) or at the first one whose probe side gives a row (outer-first:
/// `Calls::at_row`); an EMPTY table ends that re-open before any key is hashed, and every later re-open scans the probe
/// side and hashes each row's keys (ExecHashJoinOuterGetTuple), so `seen` (kept across the re-opens: no open declares
/// it) is that build. The owner walks its outer rows in one participant (program serial.rs: a nest or SubPlan step over
/// a hash probe), so the first re-open is PG's.
pub(crate) fn nest_rescans(gx: &Gx<'_, '_>, k: usize, j: usize, b: usize) -> bool {
    let ps = gx.pipelines();
    if k < ps.len() || b >= ps.len() {
        return false;
    }
    let p = &ps[k];
    let Some(Op::Join(JoinDesc::Probe { jointype, slot, keys, catalog: false, .. })) = p.ops.get(j) else { return false };
    let bound = |id: i32| gx.graph.params.iter().any(|x| x.0 == id);
    let rf = |n: types_nodes::Node<'_>| crate::effects::raise_free_each(gx, n, &bound);
    let step = |op: &Op<'_>| match op {
        Op::Filter(n) | Op::Gate(n) => !rf(*n),
        Op::Project(l) => !l.iter().all(rf),
        _ => false,
    };
    let cells = matches!(&p.source, Source::Values { lists, .. } if !lists.iter().all(|l| l.as_list().is_some_and(|r| r.iter().all(rf))));
    let held = |r: &gravity_decompose::graph::Rerun| r.pipelines.contains(&(k as u16)) || r.pipelines.contains(&(b as u16)) || gravity_decompose::graph::slot_has(&r.slots, *slot);
    matches!(jointype, JoinType::JOIN_INNER | JoinType::JOIN_SEMI | JoinType::JOIN_UNIQUE_OUTER | JoinType::JOIN_UNIQUE_INNER)
        && !gx.graph.reruns.iter().any(held)
        && side_by(gx, b, usize::MAX, false) == Side::Safe
        && !crate::ops::hashjoin::probe::opaque(gx, b)
        && !defers(gx, b, k, j)
        && (!keys.iter().all(rf) || p.ops[..j].iter().any(step) || cells || seek_keys(gx, k))
        && quiet_statement(gx)
}

/// D1016 row 2: nest inner `k`'s source is a btree walk over initdb operator classes with a key it evaluates at each seek
/// (an embedded walk's every key that is no constant or bare parameter is the host's at each open, lifetime/bound.rs
/// `per_open`): the walk's raises and calls are its seek's, made where PG's first outer fetch after a rescan makes them
/// (ExecIndexEvalRuntimeKeys), which `Calls` keeps (its `close` on the build-first path).
fn seek_keys(gx: &Gx<'_, '_>, k: usize) -> bool {
    use gravity_decompose::graph::KeyForm;
    let Source::Index { keys, indexid, iam: gravity_decompose::catalog::IndexAm::Btree, .. } = &gx.pipelines()[k].source else { return false };
    let evaluated = |v: types_nodes::Node<'_>| v.as_const().is_none() && v.as_param().is_none();
    crate::effects::initdb_index(gx, *indexid, keys)
        && keys.iter().any(|x| match x.form {
            KeyForm::Null | KeyForm::NotNull => false,
            KeyForm::Row => x.value.as_row_compare_expr().is_some_and(|rc| rc.rargs.iter().any(evaluated)),
            _ => evaluated(x.value),
        })
}

/// D1016 row 2 (rerun/params_key.rs `input_cells`): every probe of pipeline `q` takes `Order` (`seek_orders`) or, on a nest
/// inner, `Calls` (`nest_rescans`): a key its walk evaluates at the seek is evaluated where PG's outer fetch evaluates it
/// (probe.rs refuses a probe that takes neither after all).
pub(crate) fn seek_calls(gx: &Gx<'_, '_>, q: usize) -> bool {
    let ops = &gx.pipelines()[q].ops;
    ops.iter().enumerate().filter_map(|(j, op)| if let Op::Join(JoinDesc::Probe { slot, .. }) = op { Some((j, *slot)) } else { None }).all(|(j, s)| gx.life_of(q, s).is_some_and(|l| orders(gx, q, j, l.0 as usize) || nest_rescans(gx, q, j, l.0 as usize)))
}

impl Calls {
    pub(crate) fn make(px: &mut Px<'_, '_>, k: usize, j: usize, outer_first: bool) -> Calls {
        let mut word = |i: u8| px.frame_word(WordKey::Open { face: Face::Join, k, j: crate::frame::px::key_j(j), i }) as i32 * 8;
        let (mode, seen) = (word(2), word(3));
        let marks = matches!(px.gx.pipelines()[k].ops.get(j), Some(Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_RIGHT_SEMI | JoinType::JOIN_RIGHT_ANTI, .. })));
        let close = !outer_first && px.seek_keyed.contains(&k);
        Calls { mode, seen, outer_first, marks, close, shut: core::cell::Cell::new(None) }
    }

    /// D1016 row 2 (`seek_keyed`, build-first): at the prologue, before the source opens, a call that runs PG's build
    /// (`seen` clear) over an EMPTY table is PG's HJ_BUILD_HASHTABLE returning before its first outer fetch, so the walk
    /// never seeks and no key of it is evaluated (`closes`). PG keeps that one-batch table and every later call scans its
    /// outer whole (ExecReScanHashJoin: HJ_NEED_NEW_OUTER), so the call sets `seen` too.
    pub(crate) fn prologue(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, consumed: usize) {
        if !self.close {
            return;
        }
        let empty = built_empty(b, env, consumed);
        let s = env.load_word(b, self.seen);
        let first = b.icmp(IntCond::Eq, s, env.zero);
        let shut = b.int_bin(IntBinOp::And, first, empty);
        let one = b.iconst(Type::I64, 1);
        let s = b.select(shut, one, s);
        env.store_word(b, self.seen, s);
        self.shut.set(Some(shut));
    }

    /// `prologue`'s verdict: the call closes at the open.
    pub(crate) fn closes(&self) -> Option<Value> {
        self.shut.get()
    }

    /// `mode` only: `seen` outlives the open.
    pub(crate) fn open_words(&self, w: &mut OpenWords) {
        w.declare(OpenWord::open("join.kept call", Base::Words, self.mode, Type::I64, 0));
    }

    /// At every window: the call's `mode` (`I64`), read from `seen` at its first window (a build-first call runs PG's
    /// build: it sets `seen` there).
    pub(crate) fn at_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, r: Value) -> Value {
        let m = env.load_word(b, self.mode);
        let unset = b.icmp(IntCond::Eq, m, env.zero);
        let (l_set, l_on) = (b.create_block(&[]), b.create_block(&[Type::I64]));
        b.cond_br(unset, (l_set, &[]), (l_on, &[m]));
        b.switch_to(l_set);
        let s = env.load_word(b, self.seen);
        let ran = b.icmp(IntCond::Ne, s, env.zero);
        let (later, first) = (b.iconst(Type::I64, LATER), b.iconst(Type::I64, FIRST));
        let v = b.select(ran, later, first);
        env.store_word(b, self.mode, v);
        if self.marks {
            crate::lifetime::hashjoin::reset_at_first_window(b, env, r);
        }
        if !self.outer_first {
            let one = b.iconst(Type::I64, 1);
            env.store_word(b, self.seen, one);
        }
        b.br(l_on, &[v]);
        b.switch_to(l_on);
        b.block_params(l_on)[0]
    }

    /// The call's `mode` at a row lowered outside the window loop (an EPQ recheck's stored row): 0 before its first window.
    pub(crate) fn mode_now(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        env.load_word(b, self.mode)
    }

    /// Whether `mode` is not a later call's (`Bool`): an empty table ends the pipeline, as PG's build call does.
    pub(crate) fn ends_empty(&self, b: &mut FunctionBuilder<'_>, mode: Value) -> Value {
        let later = b.iconst(Type::I64, LATER);
        b.icmp(IntCond::Ne, mode, later)
    }

    /// A row reaching the probe on PG's outer-first path is the outer's first fetch: PG builds its table in this call.
    pub(crate) fn at_row(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        if self.outer_first {
            let one = b.iconst(Type::I64, 1);
            env.store_word(b, self.seen, one);
        }
    }
}

/// D1902 (served): probe `j` of pipeline `q` over build `b` takes `Order`: an INNER or SEMI probe (not psql's catalog form)
/// on PG's cost path, `q` and `b` members of one rerun set, whose outer side (`side_by` through the probe) is not quiet
/// while the build side (`b`, its keys and feeders) is, and so are `q`'s source, its feeders and the builds of its probes
/// before `j`: they run at every call whichever order PG takes, and only the build's emptiness shows. A Const source (a
/// Result's one empty row, a Function Scan's) runs nothing. D3609: nor does an index walk that raises only at its seek
/// (effects/visible/hashjoin.rs `seek_raises_only`: a computed key, PG's runtime key) at a call that does not reach it: `Order::closes`
/// closes a build-first call over an EMPTY table at the open, before the walk seeks, and an outer-first call seeks first
/// as PG's first outer fetch evaluates the key.
pub(crate) fn orders(gx: &Gx<'_, '_>, q: usize, j: usize, b: usize) -> bool {
    let p = &gx.pipelines()[q];
    let Some(Op::Join(JoinDesc::Probe { jointype, outer_first: true, catalog: false, .. })) = p.ops.get(j) else { return false };
    let quiet = |x: Option<usize>| x.is_some_and(|x| x < gx.pipelines().len() && side_by(gx, x, usize::MAX, false) == Side::Safe);
    let feeders = || feed(gx, q, p.desc.consumes.iter().copied().filter(|s| !p.ops.iter().any(|op| matches!(op, Op::Join(JoinDesc::Probe { slot, .. }) if slot == s)))).into_iter().all(|x| quiet(Some(x)));
    let source = side_by(gx, q, 0, false) == Side::Safe
        || matches!(p.source, Source::Const) && feeders()
        || p.desc.driving_site >= 0 && crate::effects::visible::hashjoin::seek_raises_only(gx, q) && feeders();
    matches!(jointype, JoinType::JOIN_INNER | JoinType::JOIN_SEMI | JoinType::JOIN_UNIQUE_OUTER | JoinType::JOIN_UNIQUE_INNER)
        && gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(q as u16)) && r.pipelines.contains(&(b as u16)))
        && side_by(gx, q, j, false) != Side::Safe
        && source
        && quiet(Some(b))
        && p.ops[..j].iter().all(|op| !matches!(op, Op::Join(JoinDesc::Probe { slot, .. }) if !quiet(gx.life_of(q, *slot).map(|l| l.0 as usize))))
}

/// D3609 (rerun/params_key.rs `input_cells`): every probe of pipeline `q` may take `Order` (`orders`), so a computed key
/// its walk evaluates at the seek is evaluated where PG's first outer fetch evaluates it; a probe that takes no `Order`
/// after all (a lazy build) refuses at its make (probe.rs).
pub(crate) fn seek_orders(gx: &Gx<'_, '_>, q: usize) -> bool {
    let ops = &gx.pipelines()[q].ops;
    ops.iter().enumerate().filter_map(|(j, op)| if let Op::Join(JoinDesc::Probe { slot, .. }) = op { Some((j, *slot)) } else { None }).all(|(j, s)| gx.life_of(q, s).is_some_and(|l| orders(gx, q, j, l.0 as usize)))
}

/// D1902 (held build): probe `j` of pipeline `q` over build `b` takes `Order` with the build's held error: an INNER or SEMI
/// probe (not psql's catalog form) on PG's cost path, `q` and `b` members of one rerun set, whose build side (`b`, its keys
/// and feeders) may raise and op 14 holds it (build_class.rs `defers`; nothing `opaque` in it, the statement's user code
/// read: `quiet_statement`), while the outer side (`side_by` through the probe: `q`'s source, feeders and earlier builds)
/// is quiet. PG builds after the outer's first row while hj_OuterNotEmpty is clear (an empty outer never builds: the held
/// error is dropped, a row raises it at the probe as op 14 does) and first once a call's outer gave a row (its build
/// raises over an empty outer too: `Order::at_window` raises the held error at such a call's first window). The quiet
/// outer shows no other order.
pub(crate) fn orders_held(gx: &Gx<'_, '_>, q: usize, j: usize, b: usize) -> bool {
    let p = &gx.pipelines()[q];
    let Some(Op::Join(JoinDesc::Probe { jointype, outer_first: true, catalog: false, .. })) = p.ops.get(j) else { return false };
    matches!(jointype, JoinType::JOIN_INNER | JoinType::JOIN_SEMI | JoinType::JOIN_UNIQUE_OUTER | JoinType::JOIN_UNIQUE_INNER)
        && b < gx.pipelines().len()
        && gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(q as u16)) && r.pipelines.contains(&(b as u16)))
        && side_by(gx, q, j, false) == Side::Safe
        && side_by(gx, b, usize::MAX, false) == Side::Raises
        && !crate::ops::hashjoin::probe::opaque(gx, b)
        && quiet_statement(gx)
        && defers(gx, b, q, j)
}

/// D1902: the probe's frame words (byte offsets): `seen`, hj_OuterNotEmpty (zero at the execution's binding, kept across
/// the set's calls), and `mode`, an open word, the call's order read from `seen` at its first window. `held`: the build's
/// error is held (`orders_held`), raised at a build-first call's first window.
pub(crate) struct Order {
    mode: i32,
    seen: i32,
    held: bool,
}

impl Order {
    pub(crate) fn make(px: &mut Px<'_, '_>, k: usize, j: usize, held: bool) -> Order {
        let mut word = |i: u8| px.frame_word(WordKey::Open { face: Face::Join, k, j: crate::frame::px::key_j(j), i }) as i32 * 8;
        let (mode, seen) = (word(4), word(5));
        Order { mode, seen, held }
    }

    pub(crate) fn open_words(&self, w: &mut OpenWords) {
        w.declare(OpenWord::open("join.member order", Base::Words, self.mode, Type::I64, 0));
    }

    /// At every window: a build-first call (`seen` set at its first window) whose table is EMPTY (`form` 0) ends before
    /// any outer row (`l_done`); at the first window a build-first call over a table with rows clears `seen`, as PG
    /// resets hj_OuterNotEmpty after its build. A held build's (`held`): a build-first call raises its held error (FORM
    /// FAILED) here, before any outer row, as PG's build raises over an empty outer too.
    pub(crate) fn at_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, form: Value, l_done: Block) {
        let m = env.load_word(b, self.mode);
        let unset = b.icmp(IntCond::Eq, m, env.zero);
        let (l_set, l_on) = (b.create_block(&[]), b.create_block(&[Type::I64]));
        b.cond_br(unset, (l_set, &[]), (l_on, &[m]));
        b.switch_to(l_set);
        let s = env.load_word(b, self.seen);
        let first = b.icmp(IntCond::Ne, s, env.zero);
        let (build, outer) = (b.iconst(Type::I64, BUILD), b.iconst(Type::I64, OUTER));
        let v = b.select(first, build, outer);
        env.store_word(b, self.mode, v);
        let empty = b.icmp(IntCond::Eq, form, env.zero);
        let s = b.select(empty, s, env.zero);
        env.store_word(b, self.seen, s);
        b.br(l_on, &[v]);
        b.switch_to(l_on);
        let m = b.block_params(l_on)[0];
        let bf = b.iconst(Type::I64, BUILD);
        let bf = b.icmp(IntCond::Eq, m, bf);
        let empty = b.icmp(IntCond::Eq, form, env.zero);
        let ends = b.int_bin(IntBinOp::And, bf, empty);
        let l_go = b.create_block(&[]);
        b.cond_br(ends, (l_done, &[]), (l_go, &[]));
        b.switch_to(l_go);
        if self.held {
            let (l_bf, l_on) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(bf, (l_bf, &[]), (l_on, &[]));
            b.switch_to(l_bf);
            crate::ops::hashjoin::probe_wait::raise_failed(b, env, form);
            b.br(l_on, &[]);
            b.switch_to(l_on);
        }
    }

    /// D3609: at the open, before the source opens (`StepMech::closes`): a build-first call (`seen` set) over an EMPTY
    /// table closes the call, so its walk never seeks and no key of it is evaluated, as PG's empty build returns before
    /// its first outer fetch (HJ_BUILD_HASHTABLE) and leaves hj_OuterNotEmpty set.
    pub(crate) fn closes(&self, b: &mut FunctionBuilder<'_>, env: &Env, consumed: usize) -> Value {
        let r = crate::ops::hashjoin::build::region(b, env, Some(consumed));
        let form = b.load_zext(Type::I8, r, crate::ops::hashjoin::build::FORM as i32);
        let s = env.load_word(b, self.seen);
        let first = b.icmp(IntCond::Ne, s, env.zero);
        let empty = b.icmp(IntCond::Eq, form, env.zero);
        b.int_bin(IntBinOp::And, first, empty)
    }

    /// A row reaches the probe over an EMPTY table (an outer-first call's first fetch), or its outer keys are non-NULL:
    /// PG sets hj_OuterNotEmpty.
    pub(crate) fn at_row(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        let one = b.iconst(Type::I64, 1);
        env.store_word(b, self.seen, one);
    }
}

/// D3373 (probe.rs `arm_close`): the call's table is built and EMPTY: no lazy build (D824) of consumed slot `consumed` is
/// pending (probe_wait.rs `wait`'s flag) and the region's form reads EMPTY.
pub(crate) fn built_empty(b: &mut FunctionBuilder<'_>, env: &Env, consumed: usize) -> Value {
    let r = crate::ops::hashjoin::build::region(b, env, Some(consumed));
    let form = b.load_zext(Type::I8, r, crate::ops::hashjoin::build::FORM as i32);
    let empty = b.icmp(IntCond::Eq, form, env.zero);
    let fl = b.load_zext(Type::I32, env.ctx, gravity_abi::ctx::hdr::FLAGS as i32);
    let bit = b.iconst(Type::I64, 1i64 << (gravity_abi::ctx::FLAG_PENDING_SHIFT as usize + consumed));
    let m = b.int_bin(IntBinOp::And, fl, bit);
    let built = b.icmp(IntCond::Eq, m, env.zero);
    b.int_bin(IntBinOp::And, empty, built)
}

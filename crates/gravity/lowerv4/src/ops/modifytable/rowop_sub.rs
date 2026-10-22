//! W6-D1112 (V4R-528): a SubPlan in an INSERT's or UPDATE's check options (a view's WITH CHECK OPTION, an RLS WITH CHECK),
//! read where the host's qual reads it, as PostgreSQL's ExecWithCheckOptions reaches ExecEvalSubPlan. A helper of
//! write/rowop_step.rs (registers nothing). The host evaluates the check over the row as written (hosts write_check.rs);
//! at the SubPlan's node its qual suspends, the row op parks and the call returns `WRITE_SUBPLAN`. The row op step then
//! loops: `write_row(.., WRITE_SUB_ARGS)` hands it the parked call (the SubPlan's plan id and its arguments, computed by the
//! host over the check's slot), the step stores the arguments into the SubPlan's parameter words, evaluates the SubPlan
//! nest.subplan lent it (`SubEval`, host-fed: nest/subplan.rs compiles no argument of its own), writes the value back and
//! resumes the row op (`op | WRITE_RESUME`), which may park again (a second SubPlan, the view check after the RLS one).
//! A SubPlan the qual never reaches (an earlier conjunct false, an OR arm that passed, an earlier check failing, a row a
//! BEFORE ROW trigger skipped) is never evaluated; one it reads twice runs twice. Refused here by name (RETURNING, ON
//! CONFLICT and MERGE keep D1112 at write/insert.rs / update.rs): a ROWCOMPARE or MULTIEXPR SubPlan (an ANY / ALL one's
//! test reads what the host lays after the arguments: a hashed ANY's probe its test's left sides, write_check.rs
//! `lay_lefts`, keeping an error until the probe proves the set non-empty, subplan's hashed.rs `Lhs::Fed`, refusing a left
//! side that calls a VOLATILE or user function or reads an executor parameter; a nested one's test the written row,
//! `lay_row`, per_row.rs `FedRow`, updatable_views#978), a SubPlan, system column or whole-row value in its arguments
//! (the check slot is virtual), an INSTEAD OF view target, a data-modifying CTE's write (a routed INSERT's leaf parks
//! through the route host, updatable_views#978; a several-target write's target through the multi-target host and a
//! moved row's destination through the move host to its source's, subplan D1112 row B: hosts write_route.rs /
//! write_move.rs). An aggregate or grouping beside a hash or merge join anywhere in the statement serves: a grouping over
//! a lazily run build keeps its end's state across the probe's resume (D2194 / D2147 / D2201, fixed; the guard stayed
//! while D2325 was open).
use gravity_abi::program::{SUB_ARGS, SUB_PLAN_ID, SUB_RESULT, SUB_RESULT_NULL, WRITE_RESUME, WRITE_SUB_ARGS};
use gravity_abi::status::{unsupported, UNSUPPORTED_SHAPE, WRITE_SUBPLAN};
use gravity_decompose::ops::shared::subplan::{SubForm, SubLink, SubPlanDesc};
use gravity_decompose::ops::shared::modifytable::{Member, WriteDesc};
use gravity_decompose::Op;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};
use gravity_program::Cmd;
use mcx::{CollectIn, PgVec};
use types_nodes::primnodes::BoolExprType;

use crate::face::kinds::{Face, KindId};
use crate::frame::env::Env;
use crate::frame::px::{Px, WordKey};
use crate::frame::row::RowVals;
use crate::graph::Gx;
use crate::admit::refusal::{Reason, ShapeWhy};

/// The check options' SubPlans of pipeline `k`'s write, each with its parameters in call order (its parParam list), for a
/// hashed one the words of its clauses' left sides the host lays after them (subplan's hashed.rs `fed_words`), and for a
/// nested ANY / ALL one the word its test reads the parked call's pointer from (the row laid after them: per_row.rs
/// `FedRow`, its op's Nest open word 5).
pub(crate) type Fed<'g> = PgVec<'g, (i32, PgVec<'g, i32>, PgVec<'g, (i32, i32)>, Option<i32>)>;

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

/// The SubPlans nest.subplan lends host-fed to pipeline `k`'s write (the ones its check options read), or a refusal by
/// name at `node`; empty for a write with none.
pub(crate) fn fed<'g>(px: &mut Px<'g, '_>, k: usize, node: i32) -> Fed<'g> {
    let gx = px.gx;
    let mcx = px.mcx();
    let p = &gx.pipelines()[k];
    let Some(w) = p.ops.iter().find_map(|o| if let Op::Write(w) = o { Some(w) } else { None }) else { return PgVec::new_in(mcx) };
    let reads = |id: i32| w.checks.iter().any(|c| gravity_decompose::ops::shared::subplan::refers(c.qual, id));
    let subs: PgVec<'_, _> = p.ops.iter().enumerate().filter_map(|(j, o)| if let Op::SubPlan(d) = o { Some((j, d)) } else { None }).filter(|(_, d)| reads(d.plan_id)).collect_in(mcx);
    if subs.is_empty() {
        return PgVec::new_in(mcx);
    }
    if let Some(why) = write_why(px, k, w) {
        shape(node, why);
    }
    for (_, d) in subs.iter() {
        // a hashed ANY's test is its probe over the left sides the host lays (write_check.rs `lay_lefts`), a nested ANY /
        // ALL one's reads the written row the host lays (`lay_row`; updatable_views#978)
        if !matches!(d.link, SubLink::Exists | SubLink::Expr | SubLink::Array | SubLink::Any | SubLink::All) || d.link == SubLink::All && !matches!(d.form, SubForm::Nested { .. }) {
            shape(node, "a check option's ROWCOMPARE or MULTIEXPR SubPlan (its test reads the written row per inner row: D1112 row B)");
        }
        for a in d.args.iter() {
            let mut why = None;
            gravity_decompose::expr::walk(a, &mut |n| {
                if n.as_sub_plan().is_some() {
                    why = Some("a SubPlan in a check option SubPlan's arguments (D1112 row B)");
                } else if n.as_var().is_some_and(|v| v.varattno <= 0) {
                    why = Some("a system column or whole-row argument of a check option SubPlan (the host's check slot is virtual: D1112 row B)");
                }
                why.is_none()
            });
            if let Some(why) = why {
                shape(node, why);
            }
        }
    }
    // a hashed one's probe reads each clause's left side (value, null) from the Nest open words 2.. of its op (subplan's
    // hashed.rs `fed_words`, one pair per clause of its test: an AND's arguments, else the test alone)
    let keys = |px: &mut Px<'_, '_>, j: usize, d: &SubPlanDesc<'_>| match d.form {
        SubForm::Hashed { .. } => {
            let and = d.test.and_then(|t| t.as_bool_expr()).filter(|be| be.boolop == BoolExprType::AND_EXPR);
            let clauses = d.test.map_or(0, |_| and.map_or(1, |be| be.args.iter().count()));
            let mut word = |i: usize| px.frame_word(WordKey::Open { face: Face::Nest, k, j: crate::frame::px::key_j(j), i: (2 + i) as u8 }) as i32 * 8;
            (0..clauses).map(|c| (word(2 * c), word(2 * c + 1))).collect_in(mcx)
        }
        _ => PgVec::new_in(mcx),
    };
    let call = |px: &mut Px<'_, '_>, j: usize, d: &SubPlanDesc<'_>| {
        (matches!(d.form, SubForm::Nested { .. }) && matches!(d.link, SubLink::Any | SubLink::All)).then(|| px.frame_word(WordKey::Open { face: Face::Nest, k, j: crate::frame::px::key_j(j), i: 5 }) as i32 * 8)
    };
    subs.iter().map(|&(j, d)| (d.plan_id, d.params.iter().collect_in(mcx), keys(px, j, d), call(px, j, d))).collect_in(mcx)
}

/// The writes whose check options' SubPlans this row does not serve.
fn write_why(px: &Px<'_, '_>, k: usize, w: &WriteDesc<'_>) -> Option<&'static str> {
    if !matches!(w.cmd, Cmd::Insert | Cmd::Update) {
        Some("a SubPlan in a write's own expressions, a check, RETURNING, ON CONFLICT or a MERGE action (D1112: nest.subplan B4)")
    } else if w.members.has(Member::TargetView) {
        Some("a check option SubPlan over an INSTEAD OF view target (D1112 row B)")
    } else if px.gx.cte_write(k) {
        Some("a check option SubPlan of a data-modifying CTE's write (D1112 row B)")
    } else {
        None
    }
}

/// After the row op's host call (status `st`): while the host parks at a check's SubPlan read, evaluate that SubPlan and
/// resume the row op; the status it ends with. `fed` empty: `st` as it came. `row`: the row op's (`SubEval`'s reader row).
pub(crate) fn serve(b: &mut FunctionBuilder<'_>, env: &mut Env, site: usize, op: u32, st: Value, (fed, row): (&Fed<'_>, &RowVals), node: i32) -> Value {
    if fed.is_empty() {
        return st;
    }
    // a checked loop: a qual may park at SubPlan reads without end (a cancel reaches the row between them)
    let l_out = b.create_block(&[Type::I64]);
    let (l_loop, l_body) = crate::frame::cancel::header(b, env.ctx, env.l_cancel, &[Type::I64], env.mcx);
    b.br(l_loop, &[st]);
    b.switch_to(l_body);
    let st = b.block_params(l_body)[0];
    let parked = b.iconst(Type::I64, WRITE_SUBPLAN);
    let is_parked = b.icmp(IntCond::Eq, st, parked);
    let l_sub = b.create_block(&[]);
    b.cond_br(is_parked, (l_sub, &[]), (l_out, &[st]));
    b.switch_to(l_sub);
    // the parked call's words (below zero: the host's error, as any status)
    let c = crate::ops::modifytable::rowop::host_op(b, env, site, WRITE_SUB_ARGS);
    let bad = b.icmp(IntCond::Slt, c, env.zero);
    let l_call = b.create_block(&[]);
    b.cond_br(bad, (l_out, &[c]), (l_call, &[]));
    b.switch_to(l_call);
    let p = b.conv(ConvOp::IntToPtr, Type::Ptr, c);
    let pid = b.load(Type::I64, p, SUB_PLAN_ID);
    for (plan_id, params, keys, call) in fed.iter() {
        let Some(&(_, vo, fo, lent)) = env.subplans.iter().find(|s| s.0 == *plan_id).filter(|s| s.3 != 0) else {
            crate::refuse!(node, Reason::Bug { payload: "a check option SubPlan nest.subplan did not lend host-fed" })
        };
        let want = b.iconst(Type::I64, *plan_id as i64);
        let hit = b.icmp(IntCond::Eq, pid, want);
        let (l_hit, l_next) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(hit, (l_hit, &[]), (l_next, &[]));
        b.switch_to(l_hit);
        for (i, &id) in params.iter().enumerate() {
            let at = SUB_ARGS + 16 * i as i32;
            let (v, n) = (b.load(Type::I64, p, at), b.load(Type::I64, p, at + 8));
            let null = b.icmp(IntCond::Ne, n, env.zero);
            crate::rerun::params::store(b, env, node, id, v, null);
        }
        for (i, &(vw, nw)) in keys.iter().enumerate() {
            let at = SUB_ARGS + 16 * (params.len() + i) as i32;
            let (v, n) = (b.load(Type::I64, p, at), b.load(Type::I64, p, at + 8));
            env.store_word(b, vw, v);
            env.store_word(b, nw, n);
        }
        if let Some(w) = *call {
            env.store_word(b, w, c);
        }
        let memo = env.memo.set_aside(); // evaluated here; its block's reads serve no read outside it
        (crate::frame::iface::kind_impl::<crate::rerun::face::SubEval>(&node, KindId::NestSubplan).0)(b, env, lent, row);
        env.memo.restore(memo);
        let (v, flags) = (env.load_word(b, vo), env.load_word(b, fo));
        let nb = b.iconst(Type::I64, crate::rerun::params::NULL);
        let n = b.int_bin(IntBinOp::And, flags, nb);
        let null = b.icmp(IntCond::Ne, n, env.zero);
        let null = b.conv(ConvOp::Zext, Type::I64, null);
        b.store(p, SUB_RESULT, v);
        b.store(p, SUB_RESULT_NULL, null);
        let again = crate::ops::modifytable::rowop::host_op(b, env, site, op | WRITE_RESUME);
        b.br(l_loop, &[again]);
        b.switch_to(l_next);
    }
    // a plan id no lent SubPlan has: the protocol's bug, unsupported
    let uns = b.iconst(Type::I64, unsupported(UNSUPPORTED_SHAPE));
    b.br(l_out, &[uns]);
    b.switch_to(l_out);
    b.block_params(l_out)[0]
}

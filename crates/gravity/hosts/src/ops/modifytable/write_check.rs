//! Rung 10, members `CheckRls` / `CheckView`: the plan's WITH CHECK OPTION quals (`ExecWithCheckOptions`) over the row's
//! cells in a virtual slot — the RLS kinds before the constraints, the view's after the write; false or NULL violates.
//! M8-WRITE W2: an InitPlan's parameter in a qual is read lazily, as PostgreSQL's ExecEvalParamExec: the qual suspends at
//! the parameter's first fetch in this execution and the Init cell runs the InitPlan then (`InitParams::fetch`, the
//! `m8_init_run` of M8-SUB's nest.initplan), so one the qual never reaches never runs and never raises.
//! W6-D1112 (V4R-528): a SubPlan in a qual (EXISTS / EXPR / ARRAY: lowerv4 write/rowop_sub.rs refuses the rest by name) is
//! read lazily too, as ExecEvalSubPlan: the qual suspends at the node, its arguments already set over the check's slot (the
//! row as written) by its ParamSet steps; the qual parks (`check_parked`), the row op hands the IR the call (`sub_call`),
//! the IR evaluates it (nest.subplan's SubEval) and the row op's resume continues the qual with its value.
//! A hashed SubPlan (ANY over the set) reads the written row through its test's left sides (PG's projLeft): the call
//! carries them after the parParams, one (value, null) per clause, evaluated here over the check's slot (`Lefts`). PG
//! evaluates them only once the set proved non-empty: an error here is kept (null word `RAISED`) and raised at the resume
//! when the probe answers NULL (the set is non-empty), dropped when it answers FALSE (an empty set).

use core::ptr::NonNull;
use std::rc::Rc;
use std::sync::Arc;

use datum::Datum;
use execexpr::{EvalSlots, ExprState, ParamBind, QualOutcome, SubplanCompileEnv, SuspendKind, Suspension};
use gravity_abi::program::{SUB_ARGS, SUB_PLAN_ID, SUB_RESULT, SUB_RESULT_NULL};
use mcx::{Mcx, PgBox};
use types_error::{PgError, PgResult, ERRCODE_INSUFFICIENT_PRIVILEGE};
use types_portal::params::ParamExecData;
use types_nodes::parsenodes::WCOKind;
use types_nodes::Node;
use types_rel::Relation;
use types_slot::SlotData;

use crate::ops::modifytable::write_constraints::ModifiedCols;

/// One check option of THIS statement: the current plan's qual (`exec/src/admit.rs` relocates it per admission).
pub struct CheckSpec {
    pub kind: WCOKind,
    pub relname: Arc<str>,
    pub polname: Option<Arc<str>>,
    pub node: Node<'static>,
    /// The result relation whose list holds it (withCheckOptionLists' index; 0 for one target).
    pub target: u16,
    /// W2: the program's InitPlans (every spec of a statement holds the same list; empty without one).
    pub inits: Rc<[InitWord]>,
}

/// W2: one InitPlan of the program (its `ProtoKind::Init` proto): exec params `first .. first + n` are its pairs, its Init
/// cell's address in parameter word `word` (exec's prepare lays it per execution).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InitWord {
    pub first: i32,
    pub n: u32,
    pub word: u16,
}

/// W2: the execution's parameter words (the binding's `params`, alive for the execution: the host keeps the address
/// between its bind and unbind only).
#[derive(Clone, Copy, Debug)]
pub struct ParamWords {
    ptr: *const u64,
    len: usize,
}

impl ParamWords {
    pub const NONE: ParamWords = ParamWords { ptr: core::ptr::null(), len: 0 };

    pub fn of(words: &[u64]) -> ParamWords {
        ParamWords { ptr: words.as_ptr(), len: words.len() }
    }

    fn get(&self, i: usize) -> Option<u64> {
        // `ptr .. len` is the binding's words, alive while the host is bound (every check runs between bind and unbind)
        super::raw::param_word(self.ptr, self.len, i)
    }
}

/// W2: the checks' executor parameters (`es_param_exec_vals` for the quals): every InitPlan param pending (`exec_plan`) at
/// the bind, set at its first fetch through the Init cell, kept for the execution as PostgreSQL keeps it.
struct InitParams {
    vals: Box<[ParamExecData]>,
    inits: Rc<[InitWord]>,
    words: ParamWords,
}

impl InitParams {
    /// W6-D1112: `subs` = the check SubPlans' parParam slots (below it; never pending: their ParamSet steps write them).
    fn new(inits: Rc<[InitWord]>, words: ParamWords, subs: usize) -> InitParams {
        let n = inits.iter().map(|i| i.first.max(0) as usize + i.n as usize).max().unwrap_or(0).max(subs);
        let mut vals = vec![ParamExecData::EMPTY; n].into_boxed_slice(); // alloc: Node
        // bounded("other: one per InitPlan the check options' quals read (the plan's initPlan list), a flag set on its parameters")
        for i in inits.iter() {
            vals[i.first.max(0) as usize..][..i.n as usize].iter_mut().for_each(|v| v.exec_plan = true);
        }
        InitParams { vals, inits, words }
    }

    fn bind(&mut self) -> ParamBind<'static> {
        if self.vals.is_empty() {
            return ParamBind::NONE;
        }
        ParamBind { extern_params: None, exec_vals: core::ptr::NonNull::new(self.vals.as_mut_ptr()), n_exec: self.vals.len() as u32, param_callback: None, param_expanded: None, param_rw: None, param_ro_expanded: None, extern_fetch: None }
    }

    /// ExecSetParamPlan for `paramid`: the Init cell runs its InitPlan once for the execution (or ran it already), then its
    /// pair is the param's value; its error (a second row: 21000) is the statement's, raised here, at the read.
    fn fetch(&mut self, paramid: i32) -> PgResult<()> {
        let Some(i) = self.inits.iter().find(|i| (i.first..i.first + i.n as i32).contains(&paramid)) else {
            return Err(Box::new(PgError::error(format!("gravity: a check option's exec param {paramid} no InitPlan sets")))); // alloc: Cold
        };
        let Some(cell) = self.words.get(i.word as usize).filter(|&c| c != 0) else {
            return Err(Box::new(PgError::error(format!("gravity: a check option's InitPlan param {paramid} without its Init cell")))); // alloc: Cold
        };
        // `cell` is the Init cell exec's prepare laid for this execution (raw.rs `init_pair`'s contract)
        let Some((value, null)) = super::raw::init_pair(cell, paramid) else {
            return Err(crate::take_any_error().unwrap_or_else(|| Box::new(PgError::error(format!("gravity: the InitPlan of exec param {paramid} failed"))))); // alloc: Cold
        };
        self.vals[paramid as usize] = ParamExecData { value: Datum::from_u64(value), isnull: null != 0, exec_plan: false };
        Ok(())
    }
}

pub struct WriteChecks {
    specs: Rc<[CheckSpec]>,
    params: InitParams,
    states: Vec<PgBox<'static, ExprState<'static>>>,
    slot: SlotData<'static>,
    union: Option<types_nodes::Bitmapset<'static>>,
    /// W6-D1112: the qual parked at a SubPlan read (its spec, the suspension) and the call's words (`SUB_*` layout).
    parked: Option<(usize, Suspension)>,
    call: Box<[u64]>,
    lefts: Lefts,
    /// The parked hashed SubPlan's left-side error, kept until the resume (`RAISED`).
    lefts_err: Option<Box<PgError>>,
    /// updatable_views#978: the slot's attno of each column of the plan's row (`lay_row`), made at the first lay.
    row_map: Option<Rc<[i16]>>,
}

/// A laid left side's null word when its evaluation raised (lowerv4 hashed.rs `RAISED`: the probe answers NULL past a
/// non-empty set, and the resume raises the kept error).
const RAISED: u64 = 2;

/// W6-D1112: each hashed check SubPlan's left sides (its node, then one state per clause of its test, in order).
type Lefts = LeftsIn<'static>;
type LeftsIn<'m> = Vec<(*const types_nodes::primnodes::SubPlan<'static>, Vec<PgBox<'m, ExprState<'m>>>)>;

/// A hashed SubPlan's test clauses' left sides (an AND's arguments in order, else the test alone; each `lhs op $key`, as
/// PG's ExecInitSubPlan reads them for projLeft); `None` for a test of another shape (lowerv4 refuses it).
fn left_sides<'a>(sp: &types_nodes::primnodes::SubPlan<'a>) -> Option<Vec<Node<'a>>> {
    let t = sp.testexpr?;
    let clauses: Vec<Node<'a>> = match t.as_bool_expr() { // alloc: Node
        Some(be) if be.boolop == types_nodes::primnodes::BoolExprType::AND_EXPR => be.args.iter().collect(), // alloc: Node
        _ => vec![t], // alloc: Node
    };
    clauses.into_iter().map(|c| c.as_op_expr().filter(|o| o.args.iter().count() == 2).and_then(|o| o.args.iter().next())).collect()
}

/// W6-D1112: the specs' SubPlans: one past their highest parParam, and their most arguments (a hashed one's left sides
/// too); `None` without one.
fn subs(specs: &[CheckSpec]) -> Option<(usize, usize)> {
    let (mut any, mut ids, mut args) = (false, 0, 0);
    // the planned qual is the implicit-AND list (`expr::walk` has no List arm): its clauses
    // bounded("other: one per clause of the statement's check option quals (the plan's WithCheckOption list), an expression walk each")
    for c in specs.iter().filter_map(|s| s.node.as_list()).flat_map(|l| l.iter()) {
        gravity_decompose::expr::walk(c, &mut |n| {
            if let Some(sp) = n.as_sub_plan() {
                any = true;
                ids = sp.parParam.iter().fold(ids, |m, p| m.max(p.max(0) as usize + 1));
                args = args.max(sp.parParam.len() + if sp.useHashTable { left_sides(sp).map_or(0, |l| l.len()) } else { 0 });
            }
            true
        });
    }
    any.then_some((ids, args))
}

impl WriteChecks {
    /// `ExecInitModifyTable`'s `ExecInitQual` per option, in `mcx`: the write host's per-bind context (reset at unbind), as
    /// the executor builds them per execution; never the node context a parked Node keeps (D263).
    /// W2: `words` = this execution's parameter words (the InitPlan params pending again at every bind: one per execution).
    pub fn bind(mcx: Mcx<'static>, rel: &Relation<'static>, specs: Rc<[CheckSpec]>, words: ParamWords) -> PgResult<WriteChecks> {
        let mut states = Vec::with_capacity(specs.len()); // alloc: Node
        let sub = subs(&specs);
        let mut params = InitParams::new(specs.first().map_or_else(|| Rc::from([]), |s| Rc::clone(&s.inits)), words, sub.map_or(0, |s| s.0));
        let bind = params.bind();
        // W6-D1112: a SubPlan compiles to its ParamSet steps and a suspending step whose cookie is the node (`sub_cookie`)
        let env = sub.map(|_| SubplanCompileEnv { estate: NonNull::dangling(), init: Some(super::raw::sub_cookie), agg: None, rtable: None, parent_subplan_tlist: None, errpos: None });
        // bounded("other: one per check option of the statement (the plan's WithCheckOption list), its qual initialized as ExecInitModifyTable does")
        for s in specs.iter() {
            // the planned qual is the implicit-AND list (`ExecInitQual`: a NULL or false clause fails the row)
            let Some(mut st) = s.node.as_list().map(|l| execexpr::exec_init_qual_subplans(mcx, l, bind, env)).transpose()?.flatten() else {
                return Err(Box::new(PgError::error("gravity: a check option qual the executor did not initialize"))); // alloc: Cold
            };
            st.arm_result_mcx(mcx);
            states.push(st); // alloc: Node
        }
        let slot = tableam::table_slot_create(mcx, rel)?;
        let call = vec![0; SUB_ARGS as usize / 8 + 2 * sub.map_or(0, |s| s.1)].into_boxed_slice(); // alloc: Node
        let lefts = match sub {
            Some(_) => lefts(mcx, &specs, bind)?,
            None => Lefts::new(), // alloc: Node
        };
        Ok(WriteChecks { specs, params, states, slot, union: None, parked: None, call, lefts, lefts_err: None, row_map: None })
    }

    /// Whether THIS statement has a check option of `kind` (H-f: MERGE fetches its old row only for one).
    pub fn has(&self, kind: WCOKind) -> bool {
        self.specs.iter().any(|s| s.kind == kind)
    }

    /// `root` = a routed row's partitioned root: a view check's DETAIL shows the row in the root's layout (ExecWithCheckOptions
    /// converts through ri_RootResultRelInfo), and `modified` is the root's numbering, as the constraints' (D636).
    pub fn check(&mut self, mcx: Mcx<'static>, row_mcx: Mcx<'_>, rel: &Relation<'static>, values: &[Datum], nulls: &[bool], kind: WCOKind, modified: Option<ModifiedCols>, root: Option<&Relation<'static>>) -> PgResult<()> {
        if self.check_parked(mcx, row_mcx, rel, values, nulls, kind, modified, root)? {
            // lowerv4 serves a check option's SubPlan at the plain row op's two check points only (write/rowop_sub.rs; D1112,
            // rxnested: an ON CONFLICT DO NOTHING insert's too)
            self.unpark(mcx);
            return Err(Box::new(PgError::error("gravity: a SubPlan in a check option qual".to_string()))); // alloc: Cold
        }
        Ok(())
    }

    /// `check`, parking at a SubPlan read (W6-D1112): `Ok(true)` = parked. The parked qual's state stays armed over
    /// `row_mcx` (the caller resets it only after the row) and the slot keeps the row; the next call of the same kind
    /// resumes it with the value the IR wrote into the call's words (`values` / `nulls` are not read again).
    pub fn check_parked(&mut self, mcx: Mcx<'static>, row_mcx: Mcx<'_>, rel: &Relation<'static>, values: &[Datum], nulls: &[bool], kind: WCOKind, modified: Option<ModifiedCols>, root: Option<&Relation<'static>>) -> PgResult<bool> {
        let WriteChecks { specs, params, states, slot, union, parked, call, lefts, lefts_err, row_map } = self;
        if !specs.iter().any(|s| s.kind == kind) {
            return Ok(false);
        }
        let (from, mut resume) = match parked.take() {
            Some((i, s)) if specs[i].kind == kind => {
                let v = datum::NullableDatum { value: Datum::from_u64(call[SUB_RESULT as usize / 8]), isnull: call[SUB_RESULT_NULL as usize / 8] != 0 };
                // a kept left-side error: the set was non-empty (NULL), so PG's projLeft raised it; FALSE: empty, never run
                if let Some(e) = lefts_err.take().filter(|_| v.isnull) {
                    states[i].arm_result_mcx(mcx);
                    exectuples::exec_clear_tuple(slot, mcx);
                    return Err(e);
                }
                (i, Some(s.resume_with(v)))
            }
            Some(_) => return Err(Box::new(PgError::error("gravity: a check option resumed at another kind"))), // alloc: Cold
            None => {
                exectuples::exec_clear_tuple(slot, mcx);
                {
                    let base = slot.base_mut();
                    let n = values.len().min(base.tts_values.len());
                    base.tts_values[..n].copy_from_slice(&values[..n]);
                    base.tts_isnull[..n].copy_from_slice(&nulls[..n]);
                }
                exectuples::exec_store_virtual_tuple(slot);
                slot.base_mut().tts_tableOid = rel.rd_id;
                (0, None)
            }
        };
        // bounded("other: one per check option of the statement of this kind (the plan's WithCheckOption list), one ExecQual over the row each, as ExecWithCheckOptions loops them")
        for (i, (s, st)) in specs.iter().zip(states.iter_mut()).enumerate().skip(from).filter(|(_, (s, _))| s.kind == kind) {
            // only the qual's Boolean escapes; the persistent frames are restored below (write_constraints' discipline)
            super::raw::arm_row(&mut **st, row_mcx);
            let pass = match qual(&mut **st, slot, params, resume.take()) {
                Ok(Err(susp)) => match lay(call, params, susp).and_then(|sp| if row_laid(sp) { lay_row(call, sp, slot, (rel, root), row_map).map(|()| None) } else { lay_lefts(call, params, lefts, slot, row_mcx, mcx, sp) }) {
                    Ok(kept) => {
                        *parked = Some((i, susp));
                        *lefts_err = kept;
                        return Ok(true);
                    }
                    Err(e) => Err(e),
                },
                Ok(Ok(b)) => Ok(b),
                Err(e) => Err(e),
            };
            st.arm_result_mcx(mcx);
            if !pass? {
                return Err(violation(mcx, s, rel, slot, modified, union, root));
            }
        }
        exectuples::exec_clear_tuple(slot, mcx);
        Ok(false)
    }

    /// W6-D1112: the parked call's words (`SUB_*` layout) for the IR; `None` when no qual is parked.
    pub fn sub_call(&mut self) -> Option<*mut u64> {
        self.parked.is_some().then(|| self.call.as_mut_ptr())
    }

    /// W6-D1112: a parked qual dropped (a row op that will not resume it): its state re-armed, the slot cleared.
    pub fn unpark(&mut self, mcx: Mcx<'static>) {
        self.lefts_err = None;
        if let Some((i, _)) = self.parked.take() {
            self.states[i].arm_result_mcx(mcx);
            exectuples::exec_clear_tuple(&mut self.slot, mcx);
        }
    }
}

/// W6-D1112: the hashed check SubPlans' left sides, compiled as the quals (an InitPlan param among them is read lazily).
fn lefts<'m>(mcx: Mcx<'m>, specs: &[CheckSpec], bind: ParamBind<'m>) -> PgResult<LeftsIn<'m>> {
    let mut out: LeftsIn<'m> = Vec::new(); // alloc: Node
    // bounded("other: one per clause of the statement's check option quals (the plan's WithCheckOption list), an expression walk each")
    for c in specs.iter().filter_map(|s| s.node.as_list()).flat_map(|l| l.iter()) {
        let mut found: Vec<&'static types_nodes::primnodes::SubPlan<'static>> = Vec::new(); // alloc: Node
        gravity_decompose::expr::walk(c, &mut |n| {
            if let Some(sp) = n.as_sub_plan().filter(|sp| sp.useHashTable) {
                found.push(sp); // alloc: Node
            }
            true
        });
        // bounded("other: one per hashed SubPlan of one check option clause (the plan's own nodes)")
        for sp in found {
            let Some(ls) = left_sides(sp) else { continue };
            let mut states = Vec::with_capacity(ls.len()); // alloc: Node
            // bounded("other: one per clause of one SubPlan's test expression (the plan's own nodes), an ExecInitExpr each")
            for l in ls {
                let Some(mut st) = execexpr::exec_init_expr(mcx, Some(l), bind)? else {
                    return Err(Box::new(PgError::error("gravity: a check option SubPlan's left side the executor did not initialize"))); // alloc: Cold
                };
                st.arm_result_mcx(mcx);
                states.push(st); // alloc: Node
            }
            out.push((sp as *const _, states)); // alloc: Node
        }
    }
    Ok(out)
}

/// W6-D1112: a hashed SubPlan's call: after its parParams, each clause's left side (value, null) over the check's slot,
/// evaluated in `row_mcx` (kept while parked, as the row's cells): lowerv4's probe reads them as PG's projLeft. lowerv4
/// serves only a left side that calls no VOLATILE function, so evaluating it before the set is built (PG: only once it
/// proved non-empty) shows nothing but its error: the first one is returned kept (this clause and the rest laid
/// `RAISED`, as PG's projection stops there), never raised here; an interrupt's (class 57) is raised at once.
fn lay_lefts<'m>(call: &mut [u64], params: &mut InitParams, lefts: &mut LeftsIn<'m>, slot: &mut SlotData<'m>, row_mcx: Mcx<'_>, mcx: Mcx<'m>, sp: &types_nodes::primnodes::SubPlan<'_>) -> PgResult<Option<Box<PgError>>> {
    let Some((_, states)) = lefts.iter_mut().find(|(p, _)| core::ptr::eq(p.cast::<()>(), (sp as *const types_nodes::primnodes::SubPlan<'_>).cast::<()>())) else { return Ok(None) };
    let at = SUB_ARGS as usize / 8 + 2 * sp.parParam.len();
    if call.len() < at + 2 * states.len() {
        return Err(Box::new(PgError::error("gravity: a check option SubPlan's left sides past its call's words"))); // alloc: Cold
    }
    let mut kept = None;
    // bounded("other: one per clause of the parked SubPlan's test expression (the plan's own nodes), its left side evaluated over the row")
    for (c, st) in states.iter_mut().enumerate() {
        if kept.is_some() {
            (call[at + 2 * c], call[at + 2 * c + 1]) = (0, RAISED);
            continue;
        }
        // the value lives in `row_mcx` until the row ends (after the parked qual resumes)
        match crate::ops::modifytable::raw::eval_in_row(&mut **st, row_mcx, mcx, |st| left(st, slot, params)) {
            Ok(v) => (call[at + 2 * c], call[at + 2 * c + 1]) = (v.value.as_u64(), v.isnull as u64),
            Err(e) if types_error::errcode_to_category(e.sqlstate()) == types_error::ERRCODE_OPERATOR_INTERVENTION => return Err(e),
            Err(e) => {
                kept = Some(e);
                (call[at + 2 * c], call[at + 2 * c + 1]) = (0, RAISED);
            }
        }
    }
    Ok(kept)
}

/// updatable_views#978: a nested ANY / ALL SubPlan, whose test lowerv4 evaluates over the written row (`lay_row`).
fn row_laid(sp: &types_nodes::primnodes::SubPlan<'_>) -> bool {
    use types_nodes::primnodes::SubLinkType;
    !sp.useHashTable && matches!(sp.subLinkType, SubLinkType::ANY_SUBLINK | SubLinkType::ALL_SUBLINK)
}

/// updatable_views#978 (D1112 row B): a nested ANY / ALL SubPlan's call: after its parParams, the written row's columns
/// (value, null) in the plan's layout (a routed row's root's, `root`; a column the leaf lacks NULL), which lowerv4's test
/// reads at each inner row (subplan per_row.rs `FedRow`), as PG's testexpr reads the check's slot there. A by-reference
/// value points into the row's cells, kept while parked. The call's words grow to hold the row.
fn lay_row(call: &mut Box<[u64]>, sp: &types_nodes::primnodes::SubPlan<'_>, slot: &SlotData<'_>, (rel, root): (&Relation<'static>, Option<&Relation<'static>>), map: &mut Option<Rc<[i16]>>) -> PgResult<()> {
    let m = Rc::clone(map.get_or_insert_with(|| match root {
        Some(r) => {
            let leaf = crate::ops::modifytable::write_route::root_map(r, rel);
            let mut m: Vec<i16> = (1..=r.rd_att.attrs.len() as i16).collect(); // alloc: Node
            if let Some(leaf) = leaf {
                m.iter_mut().for_each(|a| *a = 0);
                leaf.iter().enumerate().filter(|&(_, &x)| x > 0).for_each(|(i, &x)| m[x as usize - 1] = i as i16 + 1);
            }
            m.into() // alloc: Node
        }
        None => (1..=rel.rd_att.attrs.len() as i16).collect(), // alloc: Node
    }));
    let at = SUB_ARGS as usize / 8 + 2 * sp.parParam.len();
    if call.len() < at + 2 * m.len() {
        let mut grown = vec![0; at + 2 * m.len()].into_boxed_slice(); // alloc: Node
        grown[..call.len()].copy_from_slice(call);
        *call = grown;
    }
    let base = slot.base();
    // bounded("key-columns: two words per column of the plan's row layout, at most MaxTupleAttributeNumber (1664) columns")
    for (c, &l) in m.iter().enumerate() {
        let i = usize::try_from(l).ok().and_then(|l| l.checked_sub(1)).filter(|&i| i < base.tts_values.len() && i < base.tts_isnull.len());
        (call[at + 2 * c], call[at + 2 * c + 1]) = i.map_or((0, 1), |i| (base.tts_values[i].as_u64(), base.tts_isnull[i] as u64));
    }
    Ok(())
}

/// One left side over the check's slot, an InitPlan param's first fetch running its Init cell as `qual` does.
fn left<'m>(st: &mut ExprState<'m>, slot: &mut SlotData<'m>, params: &mut InitParams) -> PgResult<datum::NullableDatum> {
    let mut resume = None;
    // bounded("other: one pass per InitPlan parameter the expression reads for the first time (InitParams::fetch sets it for the execution, so it suspends once), then the pass that returns")
    loop {
        match execexpr::exec_eval_expr_outcome(st, &mut EvalSlots { scan: Some(slot), inner: None, outer: None }, resume.take())? {
            execexpr::EvalOutcome::Done(v) => return Ok(v),
            execexpr::EvalOutcome::Suspended(s) => match s.kind {
                SuspendKind::ParamExec(paramid) => {
                    params.fetch(paramid as i32)?;
                    resume = Some(s.resume_with(datum::NullableDatum::null()));
                }
                SuspendKind::SubPlan(_) => return Err(Box::new(PgError::error("gravity: a SubPlan in a check option SubPlan's left side"))), // alloc: Cold
            },
        }
    }
}

/// W6-D1112: the parked SubPlan's call: its plan id, then each parParam's (value, null) as its ParamSet steps wrote it
/// over the check's slot (a by-reference value points into the row's cells or the row's context, both kept while parked).
/// Its node, for `lay_lefts`.
fn lay(call: &mut [u64], params: &InitParams, s: Suspension) -> PgResult<&'static types_nodes::primnodes::SubPlan<'static>> {
    let SuspendKind::SubPlan(cookie) = s.kind else { return Err(Box::new(PgError::error("gravity: a check option parked at a parameter"))) }; // alloc: Cold
    // the cookie is raw.rs `sub_cookie`'s, a SubPlan node of a spec's qual, alive while the specs are
    let sp = super::raw::cookie_sub_plan(cookie);
    let at = SUB_ARGS as usize / 8;
    if call.len() < at + 2 * sp.parParam.len() {
        return Err(Box::new(PgError::error("gravity: a check option SubPlan past its call's words"))); // alloc: Cold
    }
    call[SUB_PLAN_ID as usize / 8] = sp.plan_id as i64 as u64;
    // bounded("other: two words per correlation parameter of the parked SubPlan (its parParam list, the plan's own)")
    for (k, id) in sp.parParam.iter().enumerate() {
        let Some(v) = params.vals.get(id as usize) else { return Err(Box::new(PgError::error("gravity: a check option SubPlan parameter outside the checks' params"))) }; // alloc: Cold
        (call[at + 2 * k], call[at + 2 * k + 1]) = (v.value.as_u64(), v.isnull as u64);
    }
    Ok(sp)
}

/// `ExecQual` with the suspension an InitPlan param's first fetch makes (W2): the Init cell runs, the fetch re-executes.
/// W6-D1112: a SubPlan read returns its suspension (`Err`), which `resume` continues.
fn qual(st: &mut ExprState<'static>, slot: &mut SlotData<'static>, params: &mut InitParams, mut resume: Option<execexpr::Resume>) -> PgResult<Result<bool, Suspension>> {
    // bounded("other: one pass per InitPlan parameter the qual reads for the first time (InitParams::fetch sets it for the execution, so it suspends once), then the pass that returns")
    loop {
        match execexpr::exec_qual_outcome(st, &mut EvalSlots { scan: Some(slot), inner: None, outer: None }, resume.take())? {
            QualOutcome::Done(b) => return Ok(Ok(b)),
            QualOutcome::Suspended(s) => match s.kind {
                SuspendKind::ParamExec(paramid) => {
                    params.fetch(paramid as i32)?;
                    resume = Some(s.resume_with(datum::NullableDatum::null()));
                }
                SuspendKind::SubPlan(_) => return Ok(Err(s)),
            },
        }
    }
}

#[cold]
#[inline(never)]
fn violation(mcx: Mcx<'static>, s: &CheckSpec, rel: &Relation<'static>, slot: &mut SlotData<'static>, modified: Option<ModifiedCols>, union: &mut Option<types_nodes::Bitmapset<'static>>, root: Option<&Relation<'static>>) -> Box<PgError> {
    if s.kind != WCOKind::WCO_VIEW_CHECK {
        // ExecWithCheckOptions' texts per kind (execMain.c:2329-2366): H-b adds the ON CONFLICT and MERGE USING checks
        let (who, using) = match s.kind {
            WCOKind::WCO_RLS_MERGE_UPDATE_CHECK | WCOKind::WCO_RLS_MERGE_DELETE_CHECK => ("target", " (USING expression)"),
            WCOKind::WCO_RLS_CONFLICT_CHECK => ("new", " (USING expression)"),
            _ => ("new", ""),
        };
        let msg = match s.polname.as_deref() {
            Some(p) => format!("{who} row violates row-level security policy \"{p}\"{using} for table \"{}\"", s.relname), // alloc: Cold
            None => format!("{who} row violates row-level security policy{using} for table \"{}\"", s.relname), // alloc: Cold
        };
        return Box::new(PgError::error(msg).with_sqlstate(ERRCODE_INSUFFICIENT_PRIVILEGE)); // alloc: Cold
    }
    let cols = match modified {
        Some((ins, upd)) if upd.is_empty() => Some(ins),
        Some((ins, upd)) if ins.is_empty() => Some(upd),
        Some((ins, upd)) => {
            if union.is_none() {
                *union = ins.union(upd, mcx).ok(); // alloc: Cold
            }
            union.as_ref()
        }
        None => None,
    };
    nodemodifytable::view_wco_violation(mcx, &s.relname, rel, slot, root, cols)
}

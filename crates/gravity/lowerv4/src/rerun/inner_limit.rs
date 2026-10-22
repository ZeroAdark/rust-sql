//! A nest inner's LIMIT (M8-NEST-B D1018 row A, V4R-496; a helper of nest/inner.rs, split for its budget). step/limit.rs's
//! counters are per-open words, written at every re-open with the inner's; its end, the count met at a window (the Limit's
//! window check through `Exits::l_done`) or past the row it handed on (`Env::full`), ends that re-open's walk as the
//! source's end does (a Memoize's entry completes), never the outer pipeline: `Ends` are the walk's blocks for both, and
//! nest/inner.rs's `Side::full` routes `Env::full` to them while the inner's steps lower. A stop in the owner's continuation
//! reaches the LIMIT's `through` with the inner row's cursor, not the outer row's: past the row (`row.next`, counted) unless
//! the resume re-enters it (an expanding offset of the inner's or of the owner's after the nest set, nest/loop.rs), the outer
//! cursor kept in a frame word meanwhile and handed back to the owner's stop exit (`back`). Bounds: any; one over executor
//! parameters, a negative constant or a bind-time word is read per re-open (step/limit.rs `late`, D1759); a constant count
//! of 0 over the inner's rerun set (D1903) or a one-time gate (D1917) shuts the re-open before either runs (D3378), and
//! refuses only under a Memoize (lifetime/nestloop.rs `zero_count`). D2121: a streamed union's LIMIT / OFFSET or
//! streamed group whose arms a rerun set re-runs restarts its union words at each run (`union_reset`: the union's written
//! flag is a lifetime `Scope::OwnerRun` word, stored by the set's owner before each run). Registers nothing.
//! Owner M8-NEST.
use gravity_decompose::graph::Bound;
use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::{Op, PipelineNode};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, IntBinOp, IntCond, Type, Value};
use mcx::{CollectIn, PgVec};

use crate::frame::{env::Env, frame::reached, row::{Carry, RowVals}, stop::Exits};
use crate::admit::refusal::{Reason, ShapeWhy};

/// D2121: the Shape of a rerun set re-running a union's later arm but not the pipeline whose open restarts its union words.
pub(crate) const UNION_FIRST_ARM_OUT: &str = "a rerun set re-running a streamed union's arms whose LIMIT / OFFSET or streamed group counts across them, but not its first arm's pipeline (the union words restart there at each run, as PG's ExecReScanLimit: D2121)";

/// D2121 (P0; PG 18.6's ExecReScanAppend re-runs every arm and ExecReScanLimit resets its counters at every rescan): the
/// ops past a streamed union's arm marker (op `j` of pipeline `k`, the Append's plan node `node`) keep a LIMIT / OFFSET's
/// or a streamed group's counters in union words (step/limit.rs, group/stream.rs: `Scope::Union`), written while the
/// union's flag word is clear (frame/open.rs `write_all`) and the flag set then, so a rerun set's later run started from
/// the last run's counts (the count already met: no row). The flag lives for one run of the set holding the union's first
/// pipeline (`run`'s first: arm 0, src.append's `top_run`): it is declared as the lifetime home's `Scope::OwnerRun` word,
/// which the set's owner stores (clear) just before every run (rerun/sets.rs `Rerun::run`), so the words start again at
/// each run and the arms, run in program order, write them once and keep counting across the arms. The first arm's
/// pipeline stays serial (src.append's `Arm::serial`): one open per run. `true` at that pipeline; a set holding a later
/// arm but not the first, an inner union's marker (no run of its own here) or arms that are not one run refuse by name.
pub(crate) fn union_reset(px: &mut crate::frame::px::Px<'_, '_>, (k, j): (usize, usize), node: i32, run: Option<(usize, usize)>, top: bool) -> bool {
    let gx = px.gx;
    let ps = gx.pipelines();
    let member = |q: usize| gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(q as u16)));
    // the union words: a LIMIT / OFFSET or a streamed group past this marker, before the next (an outer union's) marker
    let after = ps[k].ops[j + 1..].iter().take_while(|o| !matches!(o, Op::Append(AppendDesc::Arm { .. })));
    if px.tailing() || k >= ps.len() || !after.map(crate::graph::step_kind).any(|x| matches!(x, crate::face::kinds::KindId::StepLimit | crate::face::kinds::KindId::RunsGroup)) {
        return false;
    }
    match run {
        Some((first, len)) if top => {
            if k != first || !(first..first + len).any(member) {
                return false;
            }
            let Some(set) = gx.graph.reruns.iter().position(|r| r.pipelines.contains(&(first as u16))) else {
                crate::refuse!(node, Reason::Shape { why: ShapeWhy(UNION_FIRST_ARM_OUT) });
            };
            let off = px.frame_word(crate::frame::px::WordKey::UnionFlag { union: node as u32 }) as i32 * 8;
            let flag = crate::lifetime::Declared { what: UNION_FLAG, scope: crate::lifetime::Scope::OwnerRun { set: set as u16 }, place: crate::lifetime::Place::Word { off, ty: gravity_ir::Type::I64, init: 0 } };
            if px.lifetimes.declare(flag).is_err() {
                crate::refuse!(node, Reason::Bug { payload: "a streamed union's written flag declared after its rerun set's owner captured its run (D2121)" });
            }
            true
        }
        _ if member(k) => crate::refuse!(node, Reason::Shape { why: ShapeWhy(UNION_FIRST_ARM_OUT) }),
        _ => false,
    }
}

/// The union's written flag's role (one per union: frame/px.rs `WordKey::UnionFlag`).
const UNION_FLAG: &str = "a streamed union's written flag (D2121)";

/// D1903 (computed road): whether inner `ip` holds a LIMIT whose count may be 0 at a re-open: a parameter, a computed or
/// bind-time bound, a negative constant (read per re-open, step/limit.rs `late`; nest/inner.rs reads it before the set
/// runs), or a constant 0 (D3378: limit/lower.rs `reopen` answers it as a constant shut). ALL and a positive constant
/// never stop.
pub(crate) fn may_zero(ip: &PipelineNode<'_>) -> bool {
    crate::frame::iface::limit_count_late(&ip.ops) || ip.ops.iter().any(|o| matches!(crate::frame::iface::limit_bounds(o), Some((Bound::Const(0), _))))
}

/// `Side::full`: `Env::full`'s exits (`Exits::l_full`, `CarryExits::l_full`) and the stop exit (`Exits::l_stop_in`).
pub(crate) type Exit = (Block, Option<Block>, Block);

/// nest/inner.rs `swap`'s `Side::full`: the exits set to `(x, c, stop)` (`c` only under carry exits); the old returned.
pub(crate) fn swap_full(env: &mut Env, (x, c, stop): Exit) -> Exit {
    let old = (core::mem::replace(&mut env.x.l_full, x), env.carry_exits.map(|e| e.l_full), core::mem::replace(&mut env.x.l_stop_in, stop));
    if let (Some(e), Some(c)) = (env.carry_exits.as_mut(), c) {
        e.l_full = c;
    }
    old
}

/// The walk's blocks for a LIMIT's end: `end(carry..)`, `window()` (the Limit's `Exits::l_done`), `full(out, cur, accs..)`
/// (`Env::full`'s, the row's cursor the inner's already: `set_cursor` runs before the hand-on); `back(out, cur, status)`
/// the inner steps' stop exit, to the owner's (`stop`) with the outer cursor from `word`.
#[derive(Clone, Copy)]
pub(crate) struct Ends {
    end: Block,
    window: Block,
    full: Block,
    back: Block,
    stop: Block,
    word: i32,
}

impl Ends {
    pub(crate) fn new(b: &mut FunctionBuilder<'_>, env: &Env, ctypes: &[Type], word: i32) -> Ends {
        let (end, window) = (b.create_block(ctypes), b.create_block(&[]));
        let full = b.create_block(&mcx::concat_in(&[&[Type::I64][..], ctypes], env.mcx)[..]);
        let back = b.create_block(&[Type::I64, Type::I64, Type::I64]);
        b.mark_cold(back);
        Ends { end, window, full, back, stop: env.x.l_stop_in, word }
    }

    /// `Side::full` under the window's carry exits or without them.
    pub(crate) fn side(&self, env: &Env) -> Exit {
        (if env.carry_exits.is_some() { env.x.l_full } else { self.full }, env.carry_exits.map(|_| self.full), self.back)
    }

    /// The inner steps' exits: `l_done` the window's end.
    pub(crate) fn exits(&self, x: &Exits) -> Exits {
        Exits { l_done: self.window, ..*x }
    }

    /// At a hand-on, under the inner's environment: its steps' stop exit (a LIMIT's `through`) and the offset words whose
    /// being set makes the resume re-enter the inner row.
    pub(crate) fn at_row<'e>(&self, env: &Env<'e, '_>) -> (Block, PgVec<'e, i32>) {
        (env.x.l_stop_in, env.expands.iter().map(|e| e.1).collect_in(env.mcx))
    }

    /// Under the owner's environment: its continuation's stop exit set to a translation block, the owner's returned.
    pub(crate) fn tr_begin(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> (Block, Block) {
        let tr = b.create_block(&[Type::I64, Type::I64, Type::I64]);
        b.mark_cold(tr);
        (tr, core::mem::replace(&mut env.x.l_stop_in, tr))
    }

    /// The translation (`tr`): the outer cursor kept, the inner steps' stop exit taken with row `r`'s cursor.
    pub(crate) fn tr_end(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, (tr, owner): (Block, Block), (stop, offs): (Block, PgVec<'_, i32>), r: &RowVals) {
        env.x.l_stop_in = owner;
        if !reached(b, tr) {
            return;
        }
        let here = b.current();
        b.switch_to(tr);
        let p = mcx::to_vec_in(b.block_params(tr), env.mcx);
        env.store_word(b, self.word, p[1]);
        let mut set: Option<Value> = None;
        for o in offs {
            let w = env.load_word(b, o);
            set = Some(set.map_or(w, |a| b.int_bin(IntBinOp::Or, a, w)));
        }
        let cur = match set {
            Some(s) => {
                let again = b.icmp(IntCond::Ne, s, env.zero);
                b.select(again, r.at, r.next)
            }
            None => r.next,
        };
        b.br(stop, &[p[0], cur, p[2]]);
        b.switch_to(here);
    }

    /// After the steps' window checks: a count met there ends the walk with the window's carry `wc`.
    pub(crate) fn window(&self, b: &mut FunctionBuilder<'_>, wc: &Carry) {
        if reached(b, self.window) {
            let here = b.current();
            b.switch_to(self.window);
            b.br(self.end, &wc.params());
            b.switch_to(here);
        }
    }

    /// The ends, lowered: to `l_done`, through the Memoize's done (the entry complete) when there is one; `back` to the
    /// owner's stop exit with the outer cursor.
    pub(crate) fn seal(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits, memo: Option<&crate::ops::memoize::lower::Memo>, l_done: Block) {
        if reached(b, self.back) {
            b.switch_to(self.back);
            let p = mcx::to_vec_in(b.block_params(self.back), env.mcx);
            let cur = env.load_word(b, self.word);
            b.br(self.stop, &[p[0], cur, p[2]]);
        }
        if reached(b, self.full) {
            b.switch_to(self.full);
            let p = mcx::to_vec_in(b.block_params(self.full), env.mcx);
            b.br(self.end, &mcx::concat_in(&[&p[..1], &p[2..]], env.mcx)[..]);
        }
        if !reached(b, self.end) {
            return;
        }
        b.switch_to(self.end);
        let c = Carry::from_params(b.block_params(self.end), env.mcx);
        match memo {
            Some(mo) => {
                let (t, z) = (b.bconst(true), env.zero);
                mo.done(b, env, (t, z, x.l_exit), (l_done, &c));
            }
            None => b.br(l_done, &c.params()),
        }
    }
}

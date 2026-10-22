//! The lifetime home's runtime half (option 2; the user, 10-04): every word or object a lowered program keeps across rows,
//! calls or re-runs declares the scope it lives for, and the runtime resets it at the start of every instance of that
//! scope, never at whichever hook happens to run first (a window hook, a union's first open). The plan-time half, what PG
//! keeps or rebuilds at a rescan, is decompose's `lifetime`. A site that still resets outside the two halves is a row of
//! lint/LIFETIME-DEBT (rule RL); the row goes when its owner declares the state here.
//! When each executor-parameter word or bound is evaluated (the bound-at phase, C4) is `bound`'s.
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntCond, Type, Value};

pub mod bound;
pub(crate) mod cte;
pub(crate) mod hashjoin;
pub(crate) mod material;
pub(crate) mod memoize;
pub(crate) mod nestloop;
pub(crate) mod setop;
pub(crate) mod store;
pub(crate) mod subplan;

/// How long a word or object lives, widest first. Each instance of a scope starts with every state declared at it reset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    /// The prepared statement: built once, no execution resets it (a program cache entry).
    Statement,
    /// One execution of the statement, a portal's run (a hashed SubPlan's table, built once per SubPlanState).
    Execution,
    /// One run of owner rerun set `set` (its `graph.reruns` index): PG's ExecReScan of the node holding the state, at a
    /// nest's inner re-open, a SubPlan's call or a recursive generation (a kept table's match flags, which
    /// ExecHashTableResetMatchFlags clears; a LIMIT's counters past a union's first arm, which recompute_limits resets).
    OwnerRun { set: u16 },
    /// One re-open of instance `at` of nest inner `inner` (its Face::Nest index): PG's ExecReScan of the inner's subtree at
    /// each outer row (ExecReScanNestLoop), around the inner's own opens and rows (lifetime/nestloop.rs). The C4 IDENTITY
    /// instance id: `at` is the identity the inner's makers declare under (frame/px.rs `Px::ident` of its outer pipeline),
    /// so an inner made again for another program pipeline (a spill pair pass's SubPlan steps, V4R-499) is another
    /// instance with its own words (D3936); a word both instances share (a re-open's gate, position or LIMIT cursor) is
    /// declared at each (`Lifetimes::declare`).
    NestOpen { inner: u16, at: u16 },
    /// One open of the function that reads it (the frame's open words: a LIMIT's counters, a walk's state).
    Open,
    /// One input row.
    Row,
}

impl Scope {
    const fn rank(self) -> u8 {
        match self {
            Scope::Statement => 0,
            Scope::Execution => 1,
            Scope::OwnerRun { .. } | Scope::NestOpen { .. } => 2,
            Scope::Open => 3,
            Scope::Row => 4,
        }
    }

    /// A state of this scope is reset when an instance of `at` starts: `at` is this scope or a wider one. Owner runs of
    /// two sets are not ordered here (a set nested in another is the rerun home's to say): only the same set's run resets;
    /// so are re-opens of two nest inners, and an owner run against a re-open (an inner inside a set's subtree is the rerun
    /// and nest homes' to say).
    pub fn reset_by(self, at: Scope) -> bool {
        match (self, at) {
            (Scope::OwnerRun { set: a }, Scope::OwnerRun { set: b }) => a == b,
            (Scope::NestOpen { inner: a, at: x }, Scope::NestOpen { inner: b, at: y }) => a == b && x == y,
            (Scope::OwnerRun { .. }, Scope::NestOpen { .. }) | (Scope::NestOpen { .. }, Scope::OwnerRun { .. }) => false,
            _ => at.rank() <= self.rank(),
        }
    }

    /// D3936: this scope and `o` are re-opens of two instances of one nest inner.
    const fn other_instance(self, o: Scope) -> bool {
        matches!((self, o), (Scope::NestOpen { inner: a, at: x }, Scope::NestOpen { inner: b, at: y }) if a == b && x != y)
    }
}

/// When a frame per-open word (frame/open.rs `OpenWord`, written by `OpenWords::write_all` at its function's open) is
/// stored: the two forms of an open's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenAt {
    /// At every open of the function that reads it (`Scope::Open`).
    Open,
    /// Once per run of a streamed union, by the first of its arms to open (D2121: a LIMIT / OFFSET's or a streamed group's
    /// counters past the union's arm marker count across the arms, as PG's node over an Append does until its rescan). A run
    /// starts while frame word `flag` (frame/px.rs `WordKey::UnionFlag`) is clear, and its first open sets the flag
    /// (`union_run`). The flag is reset at the start of each instance of the scope holding the union: the frame words are
    /// zeroed per execution (`Scope::Execution`, the program's `SiteKind::Words`), and a union whose arms a rerun set re-runs
    /// declares its flag at that set's owner run (`Scope::OwnerRun`, rerun/inner_limit.rs `union_reset`), which the owner
    /// stores clear before each run (`RunResets`), as PG's ExecReScanLimit resets the counters at each rescan.
    Union { flag: u16 },
}

impl OpenAt {
    /// The union run's flag word (`None`: written at every open).
    pub const fn flag(self) -> Option<u16> {
        match self {
            OpenAt::Union { flag } => Some(flag),
            OpenAt::Open => None,
        }
    }
}

/// Emits the start of a union run (`OpenAt::Union { flag }`) in frame words `words`: `store` (the run's words) runs only
/// while flag word `flag` is clear, and the flag is set then, so the union's later arms keep counting until the flag's own
/// scope resets it (see `OpenAt::Union`).
pub fn union_run(b: &mut FunctionBuilder<'_>, words: Value, flag: u16, store: impl FnOnce(&mut FunctionBuilder<'_>)) {
    let at = flag as i32 * 8;
    let set = b.load(Type::I64, words, at);
    let zero = b.iconst(Type::I64, 0);
    let first = b.icmp(IntCond::Eq, set, zero);
    let (l_write, l_join) = (b.create_block(&[]), b.create_block(&[]));
    b.cond_br(first, (l_write, &[]), (l_join, &[]));
    b.switch_to(l_write);
    store(b);
    let one = b.iconst(Type::I64, 1);
    b.store(words, at, one);
    b.br(l_join, &[]);
    b.switch_to(l_join);
}

/// The program's run-again declarations (gravity_program `lifetime`), from its final pipelines and protocols, which the
/// runtime resets exactly (driver `Entry::rerun_nested`): every member of an InitPlan's or a LAZY hashed SubPlan set's
/// on-demand closure (`members_in`, the closure exec runs) is run again by that cell or set after a re-arm; every member of
/// a CTE's pulled set that a rerun set holding the CTE also holds (exec ops/ctescan `rearmed`) by the CTE's next pull; every
/// other member of a rerun set (its Rerun protos) by the set, kept for the execution when PG keeps it (`RERUN_KEPT`, the
/// keep table's answer in decompose `graph.kept`), else reset at every run. A member's lazy build (`Pipeline::lazy_for`,
/// D824) names its probe when one runner runs both. D4237: every member of a window's pulled fill (the exact request) a rerun
/// set holds by that window's next pull, every set holding the fill holding all of them. Each set's re-arms (`SetRearms`)
/// are the cells, LAZY sets, CTEs (with their spools), windows and prunings whose pipeline it holds, in protocol order. A
/// set's run is its members less those the cells, LAZY sets, CTEs and windows it re-arms run again: a member of it declared for a runner it does not re-arm (a set nested in a cell's
/// closure) is run by the set too (`SetRearms::nested`). `Err`, a Bug by name: a pipeline two runners would run again
/// (rerun/sets.rs refuses a shared member by D_RERUN_SHARED, and an on-demand closure is exclusive); a set that re-arms a
/// cell or LAZY set but does not hold every member of its closure (it would run them under the re-arm's stale params); a
/// set holding a window's pulled fill but not every member of its pulled set, or a member another set holds.
/// D4120: `refills` (decompose `graph.refills`, set index pairs) names the nested sets each set's run marks stale, by their
/// handle word (`SetRearms::refills`); a nested set with no Rerun proto is a Bug by name.
pub fn program(pipelines: &[gravity_program::Pipeline], protos: &[gravity_program::protos::Proto], refills: &[(u16, u16)]) -> Result<gravity_program::lifetime::Lifetimes, &'static str> {
    use gravity_program::lifetime::{rerun_set_of, RunLife, RunMember, Runner, SetRearms, NO_PROBE};
    use gravity_program::protos::{members_in, Proto, ProtoKind, RERUN_KEPT, SUBHASH_LAZY, WINDOW_PULLED};
    const TWO: &str = "a pipeline two runners run again (the program's lifetimes)";
    const WINDOW_PART: &str = "a rerun set holds a window's pulled fill but not every member of its pulled set, or another set holds one (the program's lifetimes, D4237)";
    const PART: &str = "a rerun set re-arms an InitPlan or LAZY hashed set without holding every member of its closure (the program's lifetimes)";
    const REFILL: &str = "a rerun set refills a nested set the program declares no Rerun proto for (the program's lifetimes, D4120)";
    fn put(ms: &mut Vec<RunMember>, pipeline: u16, runner: Runner, life: RunLife) -> Result<(), &'static str> {
        match ms.iter().find(|m| m.pipeline == pipeline) {
            Some(m) if m.runner == runner && m.life == life => Ok(()),
            Some(_) => Err(TWO),
            None => {
                ms.push(RunMember { pipeline, runner, life, lazy_probe: NO_PROBE }); // alloc: Program
                Ok(())
            }
        }
    }
    let lazy_set = |p: &Proto| p.kind == ProtoKind::SubHash && p.arg & SUBHASH_LAZY != 0;
    let pulled_window = |p: &Proto| p.kind == ProtoKind::Window && p.arg & WINDOW_PULLED != 0;
    let held = |word: u16, q: u16| protos.iter().any(|x| x.kind == ProtoKind::Rerun && x.words.base == word && x.pipeline == q);
    let mut ms: Vec<RunMember> = Vec::new(); // alloc: Program
    for p in protos.iter().filter(|p| p.kind == ProtoKind::Init || lazy_set(p)) {
        let runner = if p.kind == ProtoKind::Init { Runner::Init(p.words.base) } else { Runner::SubHash(p.words.base) };
        for m in members_in(pipelines, protos, p.pipeline as usize).members {
            put(&mut ms, m, runner, RunLife::EveryRun)?;
        }
    }
    let cells = ms.len();
    // a CTE a set re-arms: the members of its pulled set the set holds, less those of a set not holding the CTE (exec
    // ops/ctescan `members` / `rearmed`); a member a cell runs again stays the cell's
    for c in protos.iter().filter(|c| c.kind == ProtoKind::Cte) {
        let words = || protos.iter().filter(|x| x.kind == ProtoKind::Rerun && x.pipeline == c.pipeline).map(|x| x.words.base);
        let other = |q: u16| protos.iter().any(|x| x.kind == ProtoKind::Rerun && x.pipeline == q && !words().any(|s| s == x.words.base));
        for m in members_in(pipelines, protos, c.pipeline as usize).members {
            if !other(m) && words().any(|w| held(w, m)) && !ms[..cells].iter().any(|x| x.pipeline == m) {
                put(&mut ms, m, Runner::Cte(c.words.base), RunLife::EveryRun)?;
            }
        }
    }
    // D4237: a window's pulled fill a set holds (lowerv4 ops/window/lower.rs `rearmed`): the members of its pulled set, each
    // held by every set holding the fill and by no other; the window's next pull after the set's re-arm runs them (exec
    // ops/window/proto.rs `rearm`); a member a cell runs again stays the cell's
    for w in protos.iter().filter(|w| pulled_window(w)) {
        let words = || protos.iter().filter(|x| x.kind == ProtoKind::Rerun && x.pipeline == w.pipeline).map(|x| x.words.base);
        if words().next().is_none() {
            continue;
        }
        let other = |q: u16| protos.iter().any(|x| x.kind == ProtoKind::Rerun && x.pipeline == q && !words().any(|s| s == x.words.base));
        for m in members_in(pipelines, protos, w.pipeline as usize).members {
            if other(m) || !words().all(|s| held(s, m)) {
                return Err(WINDOW_PART);
            }
            if !ms[..cells].iter().any(|x| x.pipeline == m) {
                put(&mut ms, m, Runner::Window(w.words.base), RunLife::EveryRun)?;
            }
        }
    }
    // a set's member a cell, LAZY set, CTE or window runs again is only re-armed by the set (exec rerun `run` runs none of
    // them)
    let declared = ms.len();
    for p in protos.iter().filter(|p| p.kind == ProtoKind::Rerun) {
        if !ms[..declared].iter().any(|m| m.pipeline == p.pipeline) {
            let life = if p.arg & RERUN_KEPT != 0 { RunLife::FirstRun } else { RunLife::EveryRun };
            put(&mut ms, p.pipeline, Runner::Set(rerun_set_of(p.arg) as u16), life)?;
        }
    }
    // each set's re-arms, by set index (one handle word per set: exec rerun `lay` checks it)
    let nsets = protos.iter().filter(|p| p.kind == ProtoKind::Rerun).map(|p| rerun_set_of(p.arg) as usize + 1).max().unwrap_or(0);
    let mut rearms: Vec<SetRearms> = Vec::new(); // alloc: Program
    for set in 0..nsets {
        let Some(w) = protos.iter().find(|p| p.kind == ProtoKind::Rerun && rerun_set_of(p.arg) == set as u64).map(|p| p.words.base) else {
            rearms.push(SetRearms::default()); // alloc: Program
            continue;
        };
        let holds = |p: &&Proto| held(w, p.pipeline);
        let closed = |p: &Proto| members_in(pipelines, protos, p.pipeline as usize).members.iter().all(|&m| held(w, m));
        let inits = || protos.iter().filter(|p| p.kind == ProtoKind::Init).filter(holds);
        let subhash = || protos.iter().filter(|p| lazy_set(p)).filter(holds);
        if !inits().chain(subhash()).all(|p| closed(p)) {
            return Err(PART);
        }
        let ctes = || protos.iter().filter(|p| p.kind == ProtoKind::Cte).filter(holds);
        let spools = ctes().flat_map(|c| protos.iter().filter(move |x| x.kind == ProtoKind::Spool && x.pipeline == c.pipeline)).map(|x| x.words.base);
        // the set's run: its members less the closures of the cells and LAZY sets it re-arms and the pulled members of the
        // CTEs it re-arms that no set without the CTE holds (exec ops/ctescan `members`); a run member not declared the
        // set's is a runner's the set runs inside, run by the set too, kept when a member proto says so
        let mut own: Vec<u16> = protos.iter().filter(|p| p.kind == ProtoKind::Rerun && p.words.base == w).map(|p| p.pipeline).collect(); // alloc: Program
        own.sort_unstable();
        own.dedup();
        let in_closure = |q: u16| inits().chain(subhash()).any(|p| members_in(pipelines, protos, p.pipeline as usize).members.contains(&q));
        let pulled = |q: u16| {
            ctes().any(|c| {
                let other = |x: &Proto| x.kind == ProtoKind::Rerun && x.pipeline == q && !protos.iter().any(|y| y.kind == ProtoKind::Rerun && y.words.base == x.words.base && y.pipeline == c.pipeline);
                !protos.iter().any(other) && members_in(pipelines, protos, c.pipeline as usize).members.contains(&q)
            })
        };
        let kept = |q: u16| protos.iter().any(|p| p.kind == ProtoKind::Rerun && p.words.base == w && p.pipeline == q && p.arg & RERUN_KEPT != 0);
        let runner_of = |q: u16| ms.iter().find(|m| m.pipeline == q).map(|m| m.runner);
        let windowed = |q: u16| matches!(runner_of(q), Some(Runner::Window(_)));
        let nested = own.iter().copied().filter(|&q| !in_closure(q) && !pulled(q) && !windowed(q) && runner_of(q) != Some(Runner::Set(set as u16)));
        let nested = nested.map(|q| (q, if kept(q) { RunLife::FirstRun } else { RunLife::EveryRun })).collect(); // alloc: Program
        let word_of = |s: u16| protos.iter().find(|p| p.kind == ProtoKind::Rerun && rerun_set_of(p.arg) == s as u64).map(|p| p.words.base);
        let refills = refills.iter().filter(|&&(a, _)| a as usize == set).map(|&(_, b)| word_of(b).ok_or(REFILL)).collect::<Result<_, _>>()?; // alloc: Program
        let r = SetRearms {
            inits: inits().map(|p| (p.words.base, gravity_kernels::ops::shared::object::entries::init_first(p.arg))).collect(), // alloc: Program
            subhash: subhash().map(|p| p.words.base).collect(), // alloc: Program
            ctes: ctes().map(|p| p.words.base).collect(), // alloc: Program
            spools: spools.collect(), // alloc: Program
            windows: protos.iter().filter(|p| pulled_window(p)).filter(holds).map(|p| p.words.base).collect(), // alloc: Program
            prunes: protos.iter().filter(|p| p.kind == ProtoKind::Prune).filter(holds).map(|p| p.words.base).collect(), // alloc: Program
            nested,
            refills,
        };
        rearms.push(r); // alloc: Program
    }
    for i in 0..ms.len() {
        let m = ms[i];
        let runner = |q: u16| ms.iter().find(|x| x.pipeline == q).map(|x| x.runner);
        let probe = pipelines.get(m.pipeline as usize).and_then(|d| u16::try_from(d.lazy_for).ok()).filter(|&q| runner(q) == Some(m.runner));
        ms[i].lazy_probe = probe.unwrap_or(NO_PROBE);
    }
    gravity_program::lifetime::Lifetimes::new(ms).map(|l| l.with_rearms(rearms)).ok_or(TWO)
}

/// The keep rule's answer for graph pipeline `p` at a re-open of owner (`node_id`, `plan_id`: a nest, `plan_id` 0, a
/// recursive union's term, `plan_id` RECURSIVE_SET, or a nested SubPlan, as `graph.reruns` names it): why the rows the
/// owner's inner reads from it are the rows PG's rescan returns (decompose `lifetime::KeptBy`, recorded in
/// `graph.kept_by`). Pipelines mapped under the inner are answered, and so are those of a CTE a CteScan of the inner
/// reads (the answer at that CteScan). None: no answer, which a caller reads as "not known to be kept" (fail closed): the
/// owner's set runs `p` again, or `p` was mapped outside the inner and is no such CTE's (an InitPlan's pipeline, a
/// statement-level fill), or the CteScans of the inner that read the CTE disagree, or the owner declared no set.
pub fn kept_input(graph: &gravity_decompose::graph::PipelineGraph<'_>, node_id: i32, plan_id: i32, p: usize) -> Option<gravity_decompose::lifetime::KeptBy> {
    let owner = |n: i32, s: i32| n == node_id && s == plan_id;
    if graph.reruns.iter().any(|r| owner(r.node_id, r.plan_id) && r.pipelines.iter().any(|&q| q as usize == p)) {
        return None;
    }
    graph.kept_by.iter().find(|k| owner(k.node_id, k.plan_id) && k.pipeline as usize == p).map(|k| k.by)
}

/// Where a declared state lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    /// A frame word (`SiteKind::Words`) at byte offset `off`, stored with `init` of type `ty`.
    Word { off: i32, ty: Type, init: i64 },
    /// A word of the program's params area (`Env::params`: the protos' handle and extra words, one area for every function
    /// of the program) at byte offset `off`, stored with `init` of type `ty` (a CTE's eager store count, D1385).
    Param { off: i32, ty: Type, init: i64 },
    /// An object its owner resets when the runtime lists it for a scope's start (`Lifetimes::objects`): `key` is the
    /// owner's name for it (a kept table's slot).
    Object { key: u32 },
}

/// One declared state: `what` names it in a refusal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Declared {
    pub what: &'static str,
    pub scope: Scope,
    pub place: Place,
}

/// Two declarations that cannot both hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conflict {
    /// One place declared with two scopes: one of the resets would be wrong.
    TwoScopes { what: &'static str },
    /// One place holding two roles (D482's form).
    TwoRoles { what: &'static str, other: &'static str },
    /// A word declared at an owner run or a nest inner's re-open whose resets its owner already captured
    /// (`Lifetimes::owner_run`, `nest_open`): its reset would be missing from the owner's code.
    Sealed { what: &'static str },
}

/// A program's declared states.
#[derive(Default, Debug)]
pub struct Lifetimes {
    decls: Vec<Declared>,
    /// The owner runs and nest re-opens whose resets an owner captured (`owner_run`, `nest_open`).
    sealed: Vec<Scope>,
}

/// The words one run of an owner's rerun set starts with (`Scope::OwnerRun { set }`), captured when the owner declares its
/// set (`Lifetimes::owner_run`; every member pipeline is made before its owner) and stored by the owner just before the
/// set runs (rerun/sets.rs `Rerun::run`), as PG's ExecReScan resets each node's state at the start of the rescan: frame
/// words (`Place::Word`) and params words (`Place::Param`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunResets {
    words: Vec<(i32, Type, i64)>,
    params: Vec<(i32, Type, i64)>,
}

impl RunResets {
    pub fn is_empty(&self) -> bool {
        self.words.is_empty() && self.params.is_empty()
    }

    /// Stores every captured word's initial value, the frame words into `words` (`None`: the function has no words site),
    /// then the params words into `params`. `false`, nothing stored, when a frame word has no words site to go to.
    #[must_use]
    pub fn write(&self, b: &mut FunctionBuilder<'_>, words: Option<Value>, params: Value) -> bool {
        match words {
            Some(base) => {
                for &(off, ty, init) in self.words.iter() {
                    let v = b.iconst(ty, init);
                    b.store(base, off, v);
                }
            }
            None if !self.words.is_empty() => return false,
            None => {}
        }
        for &(off, ty, init) in self.params.iter() {
            let v = b.iconst(ty, init);
            b.store(params, off, v);
        }
        true
    }
}

impl Lifetimes {
    /// Declares `d`; one role declared twice (a union's arms, a re-made member after its owner captured the run) is one
    /// declaration. A word two instances of one nest inner share (D3936: the re-open's gate, position or LIMIT cursor, keyed
    /// by the inner, where its steps' words are keyed by the instance) is declared at each instance's re-open, so each
    /// captures it.
    pub fn declare(&mut self, d: Declared) -> Result<(), Conflict> {
        if self.decls.contains(&d) {
            return Ok(());
        }
        if matches!(d.scope, Scope::OwnerRun { .. } | Scope::NestOpen { .. }) && self.sealed.contains(&d.scope) {
            return Err(Conflict::Sealed { what: d.what });
        }
        match self.decls.iter().find(|o| o.place == d.place || same_slot(o.place, d.place)) {
            Some(o) if o.what != d.what => Err(Conflict::TwoRoles { what: d.what, other: o.what }),
            Some(o) if o.place == d.place && o.scope.other_instance(d.scope) => {
                self.decls.push(d); // alloc: Program
                Ok(())
            }
            Some(o) if o.scope != d.scope => Err(Conflict::TwoScopes { what: d.what }),
            Some(_) => Ok(()),
            None => {
                self.decls.push(d); // alloc: Program
                Ok(())
            }
        }
    }

    /// The words declared at owner run `set`, which its owner stores before every run; the set is sealed from then on (a later
    /// declaration at it is a `Conflict::Sealed`). An owner declared twice (a re-made inner) captures the same words.
    pub fn owner_run(&mut self, set: u16) -> RunResets {
        self.capture(Scope::OwnerRun { set })
    }

    /// The words declared at the re-open of instance `at` of nest inner `inner`, which the instance stores at every re-open
    /// (rerun/inner.rs `open`, through lifetime/nestloop.rs); sealed from then on, as `owner_run`.
    pub fn nest_open(&mut self, inner: u16, at: u16) -> RunResets {
        self.capture(Scope::NestOpen { inner, at })
    }

    fn capture(&mut self, at: Scope) -> RunResets {
        if !self.sealed.contains(&at) {
            self.sealed.push(at); // alloc: Program
        }
        let words = self.decls.iter().filter(|d| d.scope == at).filter_map(|d| match d.place {
            Place::Word { off, ty, init } => Some((off, ty, init)),
            Place::Param { .. } | Place::Object { .. } => None,
        });
        let params = self.decls.iter().filter(|d| d.scope == at).filter_map(|d| match d.place {
            Place::Param { off, ty, init } => Some((off, ty, init)),
            Place::Word { .. } | Place::Object { .. } => None,
        });
        RunResets { words: words.collect(), params: params.collect() } // alloc: Program
    }

    /// The states an instance of `at` starts with reset, its own and every narrower scope's.
    pub fn reset_at(&self, at: Scope) -> impl Iterator<Item = &Declared> + '_ {
        self.decls.iter().filter(move |d| d.scope.reset_by(at))
    }

    /// The objects declared at exactly `at`: their owners emit the resets where the runtime starts an instance of it.
    pub fn objects(&self, at: Scope) -> impl Iterator<Item = u32> + '_ {
        self.decls.iter().filter(move |d| d.scope == at).filter_map(|d| match d.place {
            Place::Object { key } => Some(key),
            Place::Word { .. } | Place::Param { .. } => None,
        })
    }

    /// Stores every frame word declared at exactly `at` into frame `words`: emitted where the runtime starts an instance of
    /// `at` (a narrower scope's words are stored at its own start, which follows). Params words go through `RunResets`.
    pub fn write_words(&self, b: &mut FunctionBuilder<'_>, at: Scope, words: Value) {
        for d in self.decls.iter().filter(|d| d.scope == at) {
            if let Place::Word { off, ty, init } = d.place {
                let v = b.iconst(ty, init);
                b.store(words, off, v);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.decls.len()
    }

    pub fn is_empty(&self) -> bool {
        self.decls.is_empty()
    }
}

/// Two word places of one area at one offset are one slot whatever their types.
fn same_slot(a: Place, b: Place) -> bool {
    match (a, b) {
        (Place::Word { off: x, .. }, Place::Word { off: y, .. }) | (Place::Param { off: x, .. }, Place::Param { off: y, .. }) => x == y,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_ir::builder::ProgramBuilder;

    const fn word(what: &'static str, scope: Scope, off: i32) -> Declared {
        Declared { what, scope, place: Place::Word { off, ty: Type::I64, init: 0 } }
    }

    /// Each instance of a scope resets its own states and every narrower scope's; one owner run resets only its own set's.
    #[test]
    fn scopes_nest() {
        let all = [Scope::Statement, Scope::Execution, Scope::OwnerRun { set: 1 }, Scope::Open, Scope::Row];
        for (i, s) in all.iter().enumerate() {
            for (j, at) in all.iter().enumerate() {
                assert_eq!(s.reset_by(*at), j <= i, "{s:?} at {at:?}");
            }
        }
        assert!(!Scope::OwnerRun { set: 1 }.reset_by(Scope::OwnerRun { set: 2 }));
        assert!(Scope::Open.reset_by(Scope::OwnerRun { set: 2 }));
        let all = [Scope::Statement, Scope::Execution, Scope::NestOpen { inner: 1, at: 0 }, Scope::Open, Scope::Row];
        for (i, s) in all.iter().enumerate() {
            for (j, at) in all.iter().enumerate() {
                assert_eq!(s.reset_by(*at), j <= i, "{s:?} at {at:?}");
            }
        }
        assert!(!Scope::NestOpen { inner: 1, at: 0 }.reset_by(Scope::NestOpen { inner: 2, at: 0 }));
        assert!(!Scope::NestOpen { inner: 1, at: 0 }.reset_by(Scope::NestOpen { inner: 1, at: 5 }), "another instance's re-open");
        assert!(!Scope::NestOpen { inner: 1, at: 0 }.reset_by(Scope::OwnerRun { set: 1 }));
        assert!(!Scope::OwnerRun { set: 1 }.reset_by(Scope::NestOpen { inner: 1, at: 0 }));
        assert!(Scope::Open.reset_by(Scope::NestOpen { inner: 2, at: 0 }));
    }

    /// A nest inner's re-open captures its own words once (a re-made inner captures the same); a later word at it is sealed
    /// out, and another inner's re-open or a set's run never takes them.
    #[test]
    fn nest_open_captures_its_words() {
        let mut l = Lifetimes::default();
        let (i3, i4) = (Scope::NestOpen { inner: 3, at: 0 }, Scope::NestOpen { inner: 4, at: 0 });
        l.declare(word("nest inner gate", i3, 8)).unwrap();
        l.declare(word("nest inner position", i3, 16)).unwrap();
        l.declare(word("other inner gate", i4, 24)).unwrap();
        let r = l.nest_open(3, 0);
        assert_eq!(r.words, vec![(8, Type::I64, 0), (16, Type::I64, 0)]);
        assert!(r.params.is_empty());
        assert_eq!(l.nest_open(3, 0), r, "a re-made inner");
        assert_eq!(l.declare(word("nest inner gate", i3, 8)), Ok(()), "the same declaration again");
        assert_eq!(l.declare(word("late word", i3, 32)), Err(Conflict::Sealed { what: "late word" }));
        assert_eq!(l.owner_run(3).words, vec![], "a set's run is not the re-open");
        assert_eq!(l.declare(word("other late", i4, 40)), Ok(()), "inner 4 not captured yet");
    }

    /// D3936: an inner made again under another identity (a spill pair pass's remade SubPlan steps, V4R-499) is another
    /// instance: its steps' words (keyed by the instance) are its own re-open's, the words keyed by the inner (its gate) are
    /// declared at both re-opens, and the first instance's capture seals neither out.
    #[test]
    fn nest_inner_instances_capture_their_own_words() {
        let mut l = Lifetimes::default();
        let (first, pass) = (Scope::NestOpen { inner: 5, at: 2 }, Scope::NestOpen { inner: 5, at: 6 });
        l.declare(word("nest inner gate", first, 8)).unwrap();
        l.declare(word("nest.spool position", first, 16)).unwrap();
        assert_eq!(l.nest_open(5, 2).words, vec![(8, Type::I64, 0), (16, Type::I64, 0)]);
        assert_eq!(l.declare(word("nest inner gate", pass, 8)), Ok(()), "the gate both instances share");
        assert_eq!(l.declare(word("nest.spool position", pass, 80)), Ok(()), "the pass instance's own step word");
        assert_eq!(l.nest_open(5, 6).words, vec![(8, Type::I64, 0), (80, Type::I64, 0)]);
        assert_eq!(l.nest_open(5, 2).words, vec![(8, Type::I64, 0), (16, Type::I64, 0)], "the first instance's capture is unchanged");
        assert_eq!(l.declare(word("late word", pass, 88)), Err(Conflict::Sealed { what: "late word" }));
        let third = Scope::NestOpen { inner: 5, at: 9 };
        assert_eq!(l.declare(word("nest inner position", third, 16)), Err(Conflict::TwoRoles { what: "nest inner position", other: "nest.spool position" }));
        assert_eq!(l.declare(word("nest inner gate", Scope::NestOpen { inner: 6, at: 2 }, 8)), Err(Conflict::TwoScopes { what: "nest inner gate" }), "another inner's re-open");
        assert_eq!(l.declare(word("nest inner gate", Scope::Open, 8)), Err(Conflict::TwoScopes { what: "nest inner gate" }));
        assert_eq!(l.reset_at(first).count(), 2);
        assert_eq!(l.reset_at(pass).count(), 2);
    }

    /// One place, one role, one scope (D2121: a union's counters live for the owner's run, not the first open; D2625: a
    /// kept table's marks for the owner's run, not its first window).
    #[test]
    fn declarations_conflict() {
        let mut l = Lifetimes::default();
        let run = Scope::OwnerRun { set: 0 };
        assert_eq!(l.declare(word("limit count", run, 8)), Ok(()));
        assert_eq!(l.declare(word("limit count", run, 8)), Ok(()), "a union's arms declare one word twice");
        assert_eq!(l.len(), 1);
        assert_eq!(l.declare(word("limit count", Scope::Open, 8)), Err(Conflict::TwoScopes { what: "limit count" }));
        assert_eq!(l.declare(word("sort run", run, 8)), Err(Conflict::TwoRoles { what: "sort run", other: "limit count" }));
        let marks = Declared { what: "kept table marks", scope: run, place: Place::Object { key: 3 } };
        assert_eq!(l.declare(marks), Ok(()));
        assert_eq!(l.declare(word("walk", Scope::Open, 16)), Ok(()));
        assert!(l.objects(run).eq([3]));
        assert!(l.objects(Scope::Open).next().is_none());
        assert_eq!(l.reset_at(run).count(), 3, "the owner's run resets its own states and the open's");
        assert_eq!(l.reset_at(Scope::Open).count(), 1);
        assert_eq!(l.reset_at(Scope::OwnerRun { set: 9 }).count(), 1, "another set's run");
    }

    /// An owner captures its set's words once its members are made; a later declaration at that run is refused, and an
    /// owner declared again (a re-made inner) captures the same words.
    #[test]
    fn owner_runs_seal() {
        let mut l = Lifetimes::default();
        let (run0, run1) = (Scope::OwnerRun { set: 0 }, Scope::OwnerRun { set: 1 });
        assert_eq!(l.declare(word("union flag", run0, 24)), Ok(()));
        assert_eq!(l.declare(word("walk", Scope::Open, 16)), Ok(()));
        assert_eq!(l.declare(word("other set's flag", run1, 32)), Ok(()));
        let store = Declared { what: "eager store count", scope: run0, place: Place::Param { off: 24, ty: Type::I64, init: 0 } };
        assert_eq!(l.declare(store), Ok(()), "a params word at a frame word's offset is another slot");
        let r = l.owner_run(0);
        assert_eq!(r.words, [(24, Type::I64, 0)], "only set 0's own words: the open's are the frame's");
        assert_eq!(r.params, [(24, Type::I64, 0)]);
        assert_eq!(l.owner_run(0), r);
        assert_eq!(l.declare(word("late flag", run0, 40)), Err(Conflict::Sealed { what: "late flag" }));
        assert_eq!(l.declare(store), Ok(()), "a re-made member's same declaration after the capture");
        let other = Declared { what: "eager store count", scope: run0, place: Place::Param { off: 32, ty: Type::I64, init: 0 } };
        assert_eq!(l.declare(other), Err(Conflict::Sealed { what: "eager store count" }));
        assert_eq!(l.declare(word("set 1 still open", run1, 48)), Ok(()));
        assert!(Lifetimes::default().owner_run(3).is_empty());
    }

    /// The program's runners: an InitPlan's closure is its cell's to run again (the set only re-arms it), a kept member
    /// lives for the execution, a lazy build names its probe when the set runs both, and the owner is no member.
    #[test]
    fn program_declares_one_runner_per_member() {
        use gravity_abi::breaker::{Breaker, BreakerKind};
        use gravity_program::lifetime::{RunLife, Runner, NO_PROBE};
        use gravity_program::protos::{Proto, ProtoKind};
        use gravity_program::{End, Pipeline, Reason};
        let agg = |slot: u8| Pipeline::serial(0, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, 64).in_slot(slot)), Reason::Host);
        let sink = Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite);
        // 0 fills slot 0 for 1, the InitPlan's cell end; 2 is a lazy build (slot 1) of 3, the probe (slot 2); 4 a kept
        // member (slot 3); 5 the owner
        let mut ps = [agg(0), sink.consuming(&[0]), agg(1), agg(2).consuming(&[1]), agg(3), sink.consuming(&[2, 3])];
        ps[2].lazy_for = 3;
        let rerun = |q: u16, set: usize, kept: bool| Proto::new(ProtoKind::Rerun, q).peer(5).words(9, 1).arg(gravity_program::lifetime::rerun_arg(set, kept));
        let mut protos = vec![Proto::new(ProtoKind::Init, 1).words(7, 1), rerun(0, 0, false), rerun(1, 0, false), rerun(2, 0, false), rerun(3, 0, false), rerun(4, 0, true)];
        let l = program(&ps, &protos, &[]).expect("one runner each");
        let at = |p: u16| l.member(p).map(|m| (m.runner, m.life, m.lazy_probe));
        assert_eq!(at(0), Some((Runner::Init(7), RunLife::EveryRun, NO_PROBE)), "the InitPlan's producer");
        assert_eq!(at(1), Some((Runner::Init(7), RunLife::EveryRun, NO_PROBE)), "its cell end");
        assert_eq!(at(2), Some((Runner::Set(0), RunLife::EveryRun, 3)), "a lazy build whose probe the set runs");
        assert_eq!(at(3), Some((Runner::Set(0), RunLife::EveryRun, NO_PROBE)));
        assert_eq!(at(4), Some((Runner::Set(0), RunLife::FirstRun, NO_PROBE)), "a member PG keeps");
        assert_eq!(at(5), None, "the owner is no member");
        protos.push(rerun(3, 1, false));
        assert!(program(&ps, &protos, &[]).is_err(), "a member two sets run");
        assert!(program(&ps, &[], &[]).is_ok_and(|l| l.is_empty()));
    }

    /// A set's re-arms are the cells, LAZY sets, CTEs (with their spools) and prunings whose pipeline it holds; a CTE's pulled
    /// members are the CTE's to run again, so the set's own run is what is left. A set re-arming a cell it holds part of the
    /// closure of is a Bug by name; a member of a cell the set does not re-arm is run by the set too.
    #[test]
    fn program_declares_each_sets_rearms() {
        use gravity_abi::breaker::{Breaker, BreakerKind};
        use gravity_program::lifetime::{rerun_arg, RunLife, Runner, SetRearms};
        use gravity_program::protos::{Proto, ProtoKind, SUBHASH_LAZY};
        use gravity_program::{End, Pipeline, Reason};
        let agg = |slot: u8| Pipeline::serial(0, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, 64).in_slot(slot)), Reason::Host);
        let sink = Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite);
        // 0 fills the InitPlan cell end 1; 2 fills the CTE 3; 4 a plain member under a pruning; 5 a LAZY hashed set's build;
        // 6 the owner
        let ps = [agg(0), sink.consuming(&[0]), agg(1), agg(2).consuming(&[1]), agg(3), agg(4), sink.consuming(&[2, 3, 4])];
        let rerun = |q: u16| Proto::new(ProtoKind::Rerun, q).peer(6).words(9, 1).arg(rerun_arg(0, false));
        let rearm = [Proto::new(ProtoKind::Init, 1).words(7, 1).arg((3u64 << 32) | 1), Proto::new(ProtoKind::Cte, 3).words(11, 1), Proto::new(ProtoKind::Spool, 3).words(12, 1), Proto::new(ProtoKind::Spool, 3).words(13, 1), Proto::new(ProtoKind::Prune, 4).words(14, 1), Proto::new(ProtoKind::SubHash, 5).words(15, 1).arg(SUBHASH_LAZY)];
        let protos: Vec<Proto> = rearm.iter().copied().chain((0..6).map(rerun)).collect();
        let l = program(&ps, &protos, &[]).expect("one runner each");
        let at = |p: u16| l.member(p).map(|m| m.runner);
        assert_eq!((at(0), at(1)), (Some(Runner::Init(7)), Some(Runner::Init(7))), "the cell's closure");
        assert_eq!((at(2), at(3)), (Some(Runner::Cte(11)), Some(Runner::Cte(11))), "the CTE's pulled set");
        assert_eq!(at(5), Some(Runner::SubHash(15)));
        assert_eq!(l.set_run(0).map(|m| (m.pipeline, m.life)).collect::<Vec<_>>(), [(4, RunLife::EveryRun)], "the set runs only what no re-arm runs");
        let want: SetRearms = SetRearms { inits: [(7, 3)].into(), subhash: [15].into(), ctes: [11].into(), spools: [12, 13].into(), windows: [].into(), prunes: [14].into(), nested: [].into(), refills: [].into() };
        assert_eq!(l.rearms(0), Some(&want));
        assert_eq!(l.rearms(1), None);
        let part: Vec<Proto> = rearm.iter().copied().chain((1..6).map(rerun)).collect();
        assert!(program(&ps, &part, &[]).is_err(), "a set re-arming a cell whose producer it does not hold");
        // a set holding a member of a cell it does not re-arm (inside the cell's closure) runs it too, as its own
        let inside: Vec<Proto> = rearm.iter().copied().chain([0, 2, 3, 4, 5].map(rerun)).collect();
        let l = program(&ps, &inside, &[]).expect("a set inside a cell's closure");
        assert_eq!(l.rearms(0).map(|r| &r.nested[..]), Some(&[(0, RunLife::EveryRun)][..]));
        assert_eq!(l.set_run(0).map(|m| (m.pipeline, m.runner)).collect::<Vec<_>>(), [(4, Runner::Set(0)), (0, Runner::Init(7))]);
        assert!(program(&ps, &protos[..1], &[]).is_ok_and(|l| l.rearms(0).is_none()), "no set");
    }

    /// D4237: a window's pulled fill a set holds, with its producer, is the window's to run again (its next pull after the
    /// set's re-arm), never the set's run; the set lists the window among its re-arms. A set holding the fill but not its
    /// producer, or another set holding the producer, is a Bug by name; an eager window's fill stays the set's.
    #[test]
    fn program_declares_a_sets_rearmed_window_fill() {
        use gravity_abi::breaker::{Breaker, BreakerKind};
        use gravity_program::lifetime::{rerun_arg, RunLife, Runner};
        use gravity_program::protos::{Proto, ProtoKind, WINDOW_PULLED};
        use gravity_program::{End, Pipeline, Reason};
        let agg = |slot: u8| Pipeline::serial(0, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, 64).in_slot(slot)), Reason::Host);
        let sink = Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite);
        // 0 the fill's producer; 1 the fill (the window's spool); 2 a set member reading another slot; 3 the owner
        let ps = [agg(0), agg(1).consuming(&[0]), agg(2), sink.consuming(&[1, 2])];
        let rerun = |q: u16, set: usize, word: u16| Proto::new(ProtoKind::Rerun, q).peer(3).words(word, 1).arg(rerun_arg(set, false));
        let win = |arg: u64| Proto::new(ProtoKind::Window, 1).words(5, 1).arg(arg);
        let protos = [win(WINDOW_PULLED), rerun(0, 0, 9), rerun(1, 0, 9), rerun(2, 0, 9)];
        let l = program(&ps, &protos, &[]).expect("one runner each");
        let at = |p: u16| l.member(p).map(|m| m.runner);
        assert_eq!((at(0), at(1)), (Some(Runner::Window(5)), Some(Runner::Window(5))), "the window's pulled set");
        assert_eq!(l.set_run(0).map(|m| (m.pipeline, m.life)).collect::<Vec<_>>(), [(2, RunLife::EveryRun)], "the set runs only what no re-arm runs");
        assert_eq!(l.rearms(0).map(|r| &r.windows[..]), Some(&[5u16][..]));
        assert!(program(&ps, &[win(WINDOW_PULLED), rerun(1, 0, 9), rerun(2, 0, 9)], &[]).is_err(), "a set holding the fill but not its producer");
        assert!(program(&ps, &[win(WINDOW_PULLED), rerun(0, 1, 20), rerun(1, 0, 9), rerun(2, 0, 9)], &[]).is_err(), "another set holding the producer");
        let eager = program(&ps, &[win(0), rerun(0, 0, 9), rerun(1, 0, 9), rerun(2, 0, 9)], &[]).expect("an eager window");
        assert_eq!(eager.set_run(0).map(|m| m.pipeline).collect::<Vec<_>>(), [0, 1, 2], "an eager window's fill is the set's");
        assert_eq!(eager.rearms(0).map(|r| r.windows.len()), Some(0));
    }

    /// D4120: a set's refills are the handle words of the nested sets decompose names (their kept fills read its params); a
    /// nested set with no Rerun proto is a Bug by name.
    #[test]
    fn program_declares_each_sets_refills() {
        use gravity_abi::breaker::{Breaker, BreakerKind};
        use gravity_program::lifetime::rerun_arg;
        use gravity_program::protos::{Proto, ProtoKind};
        use gravity_program::{End, Pipeline, Reason};
        let agg = |slot: u8| Pipeline::serial(0, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, 64).in_slot(slot)), Reason::Host);
        let sink = Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite);
        // 0 a kept member of set 0 (word 9, its owner 1); 2 the member of set 1 (word 20, its owner 3)
        let ps = [agg(0), sink.consuming(&[0]), agg(1), sink.consuming(&[1])];
        let protos = [Proto::new(ProtoKind::Rerun, 0).peer(1).words(9, 1).arg(rerun_arg(0, true)), Proto::new(ProtoKind::Rerun, 2).peer(3).words(20, 1).arg(rerun_arg(1, false))];
        let l = program(&ps, &protos, &[(1, 0)]).expect("set 1 refills set 0");
        assert_eq!(l.rearms(1).map(|r| &r.refills[..]), Some(&[9u16][..]));
        assert_eq!(l.rearms(0).map(|r| &r.refills[..]), Some(&[][..]));
        assert!(program(&ps, &protos, &[(1, 4)]).is_err(), "a refilled set with no Rerun proto");
    }

    #[test]
    fn writes_lower() {
        let r = crate::catch(7, || {
            let mut pb = ProgramBuilder::new();
            let f = pb.declare_func("p0", &[Type::Ptr], &[Type::I64]);
            let mut b = pb.define(f);
            let words = b.params()[0];
            let mut l = Lifetimes::default();
            l.declare(word("limit count", Scope::OwnerRun { set: 0 }, 8)).unwrap();
            l.declare(word("walk", Scope::Open, 16)).unwrap();
            l.write_words(&mut b, Scope::OwnerRun { set: 0 }, words);
            l.write_words(&mut b, Scope::Open, words);
            assert!(l.owner_run(0).write(&mut b, Some(words), words));
            l.declare(Declared { what: "eager store count", scope: Scope::OwnerRun { set: 1 }, place: Place::Param { off: 8, ty: Type::I64, init: 0 } }).unwrap();
            assert!(l.owner_run(1).write(&mut b, None, words), "params words alone need no words site");
            assert!(!l.owner_run(0).write(&mut b, None, words), "a frame word with no words site");
            union_run(&mut b, words, 3, |b| {
                let v = b.iconst(Type::I64, 0);
                b.store(words, 8, v);
            });
        });
        assert!(r.is_ok());
        assert_eq!(OpenAt::Union { flag: 3 }.flag(), Some(3));
        assert_eq!(OpenAt::Open.flag(), None);
    }

    /// The keep rule's recorded answer is read per owner; a pipeline the owner's set runs again, another owner's answer and
    /// a pipeline with no record all answer None (not known to be kept).
    #[test]
    fn kept_input_reads_the_owners_answer() {
        use gravity_decompose::graph::{PipelineGraph, Rerun};
        use gravity_decompose::lifetime::{KeptBy, KeptRec};
        let arena = gravity_program::Arena::new(1 << 16);
        let a = arena.handle();
        let mut g = PipelineGraph::new(a);
        let mut members = gravity_program::ArenaVec::new_in(a); // alloc: Cold
        members.push(2u16);
        g.reruns.push(Rerun { node_id: 7, plan_id: 0, pipelines: members, slots: [0; 4] });
        g.kept_by.push(KeptRec { node_id: 7, plan_id: 0, pipeline: 1, by: KeptBy::Pg { marks: true } });
        g.kept_by.push(KeptRec { node_id: 7, plan_id: 0, pipeline: 2, by: KeptBy::Quiet });
        g.kept_by.push(KeptRec { node_id: 9, plan_id: 4, pipeline: 1, by: KeptBy::Quiet });
        assert_eq!(kept_input(&g, 7, 0, 1), Some(KeptBy::Pg { marks: true }));
        assert_eq!(kept_input(&g, 7, 0, 2), None, "a member of the owner's set runs again");
        assert_eq!(kept_input(&g, 9, 4, 1), Some(KeptBy::Quiet), "another owner's answer");
        assert_eq!(kept_input(&g, 9, 0, 1), None, "a nest and a SubPlan are different owners");
        assert_eq!(kept_input(&g, 7, 0, 3), None, "no record");
    }
}

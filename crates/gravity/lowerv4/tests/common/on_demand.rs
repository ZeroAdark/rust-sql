//! M8 (RT-1): the rig's run of a program that declares Init protos (`nest.initplan`), as the server runner's `run_body` and
//! its frame (gravity_parallel `Leader`) run one: each Init's cell laid as exec's prepare lays it (`prepare_bare`), its
//! members stepped past in pipeline order (`Live::deferred`) and run on demand, nested inside their reader's call, through
//! the rig's `OnDemand` (`Entry::run_nested`). A program without an Init runs as `Entry::run` runs it. Created by M8-RUNTIME.
use gravity_driver::{Binding, Call, Entry, Exec, OnDemand, Step};
use gravity_exec::ops::Live;
use gravity_program::Program;

/// The execution's Init lives (their cells and runtime state), alive until the statement's end.
pub type Lives = [Box<dyn Live>];

/// Each Init proto's cell laid in its handle word (a kernel word the rig leaves zero): a malformed declaration is an
/// error by name, never a run over an unlaid handle.
pub fn lay(program: &Program, params: &mut [u64], nulls: &mut [u8]) -> Result<Vec<Box<dyn Live>>, String> {
    let mut nparams = params.len();
    // a cancel inside a nested run (m8_rerun's members) turns into its PgError through exec glue's abort-class check
    // (glue::pg_error): the crate has no ProcessInterrupts, so no interrupt is ever pending here; a real install stays
    // (once per test process: the tests run on parallel threads, and a second install panics)
    static SEAM: std::sync::Once = std::sync::Once::new();
    SEAM.call_once(|| {
        if !postgres_seams::check_for_abort_interrupts::is_installed() {
            postgres_seams::check_for_abort_interrupts::set(|| Ok(()));
        }
    });
    // no execution here: the lives' block is uncharged (memsig E-pub, exec `ops::LiveList`)
    let mut lives = gravity_exec::ops::LiveList::none();
    gravity_exec::ops::subplan::initplan::init::prepare_bare(program, params, nulls, &mut nparams, &mut lives).map_err(|e| format!("Init prepare: {}", e.message()))?;
    // M8 RT-2e: each rerun set's live (its members rerun through the frame's `rerun_members`), as exec's prepare lays it
    gravity_exec::rerun::prepare_bare(program, params, nulls, &mut nparams, &mut lives).map_err(|e| format!("Rerun prepare: {}", e.message()))?;
    // M8 RT-1s: each Spool proto's cell (nest.spool's record store)
    gravity_exec::ops::material::proto::prepare_bare(program, params, nulls, &mut nparams, &mut lives).map_err(|e| format!("Spool prepare: {}", e.message()))?;
    // nest.memo (M8-MEMO-CTE, MC-1): each Cache proto's memo object, as exec's prepare lays it
    gravity_exec::ops::memoize::proto::prepare_bare(program, params, nulls, &mut nparams, &mut lives).map_err(|e| format!("memo prepare: {}", e.message()))?;
    gravity_exec::ops::ctescan::proto::prepare_bare(program, params, nulls, &mut nparams, &mut lives).map_err(|e| format!("Cte prepare: {}", e.message()))?;
    Ok(lives.with_vec(core::mem::take))
}

/// write.literals (NEW-29): each VALUES pipeline's table filled from the graph's lists, as exec's prepare fills it from the
/// plan's (exec ops/modifytable/proto.rs `prepare_bare`): every VALUES list of a statement reads its Const cells from it.
pub fn lay_literals(program: &Program, graph: &gravity_decompose::PipelineGraph<'_>, params: &mut [u64], nulls: &mut [u8]) -> Result<Vec<Box<dyn Live>>, String> {
    let mut nparams = params.len();
    let lists = |k: usize| match &graph.pipelines.get(k)?.source {
        gravity_decompose::Source::Values { lists, .. } => Some(*lists),
        _ => None,
    };
    let mut lives = gravity_exec::ops::LiveList::none();
    gravity_exec::ops::modifytable::proto::prepare_bare(program, lists, params, nulls, &mut nparams, &mut lives).map_err(|e| format!("Literals prepare: {}", e.message()))?;
    Ok(lives.with_vec(core::mem::take))
}

/// The statement's pipelines to its end (`Ok(Err(..))`: the driver's error); `Err` is a step the crate does not run.
pub fn run(entry: &mut Entry, exec: &mut Exec, b: &mut Binding<'_>, cap: u32, program: &Program, lives: &Lives) -> Result<gravity_driver::Result<()>, String> {
    // every call below, the frame's nested ones included, goes through these three pointers (as `Leader` does)
    let (pe, px, pb) = (entry as *mut Entry, exec as *mut Exec, b as *mut Binding<'_>);
    let mut frame = RigFrame { entry: pe, exec: px, binding: pb, out_rows: program.layout.out_rows };
    let _published = Published::new(lives, &mut frame);
    let mut call = Call::batch(cap);
    loop {
        // SAFETY: `drive`'s entry, exec and binding outlive this loop; the frame reaches them only inside these calls.
        match unsafe { super::drive::run(&mut *pe, &mut *px, &mut *pb, call, lives) } {
            Ok(Step::Done) => return Ok(Ok(())),
            Ok(Step::Suspended { .. }) => call = Call::resume(cap),
            Ok(s) => return Err(format!("{s:?}: the crate runs no recheck")),
            Err(e) => return Ok(Err(e)),
        }
    }
}

/// The rig's `OnDemand`: an Init's members run through `Entry::run_nested`, each whole on the leader, as a producer runs
/// (exec runner/producers.rs). The fields are `run`'s objects as raw pointers: the nested run happens inside a call `run`
/// is making through the same pointers.
struct RigFrame<'b> {
    entry: *mut Entry,
    exec: *mut Exec,
    binding: *mut Binding<'b>,
    out_rows: u32,
}

impl OnDemand for RigFrame<'_> {
    fn run_members(&mut self, members: &[u16]) -> gravity_driver::Result<()> {
        // SAFETY: as `RigFrame`: `run`'s objects, alive for its whole run, used on its thread only.
        let (entry, exec, binding) = unsafe { (&mut *self.entry, &mut *self.exec, &mut *self.binding) };
        let out_rows = self.out_rows;
        let mut run = |e: &mut Entry, x: &mut Exec, bd: &mut Binding<'_>, p: usize| -> gravity_driver::Result<()> {
            let mut call = Call::batch(out_rows);
            loop {
                match e.run_pipeline(x, bd, call)? {
                    Step::Done => return super::drive::finish(e, x, bd, p),
                    Step::Suspended { .. } => call = Call::resume(out_rows),
                    s => return Err(gravity_driver::Error::host(format!("v4 rig: Init member {p} stopped on {s:?}"))),
                }
            }
        };
        entry.run_nested(exec, binding, members, &mut run)
    }

    /// M8 RT-2e: a rerun set's members run again (`Entry::rerun_nested`: each kept slot reset, then run nested), as the
    /// runner's `Leader` does.
    fn rerun_members(&mut self, members: &[u16]) -> gravity_driver::Result<()> {
        // SAFETY: as `run_members`.
        let (entry, exec, binding) = unsafe { (&mut *self.entry, &mut *self.exec, &mut *self.binding) };
        let out_rows = self.out_rows;
        let mut run = |e: &mut Entry, x: &mut Exec, bd: &mut Binding<'_>, p: usize| -> gravity_driver::Result<()> {
            let mut call = Call::batch(out_rows);
            loop {
                match e.run_pipeline(x, bd, call)? {
                    Step::Done => return super::drive::finish(e, x, bd, p),
                    Step::Suspended { .. } => call = Call::resume(out_rows),
                    s => return Err(gravity_driver::Error::host(format!("v4 rig: rerun member {p} stopped on {s:?}"))),
                }
            }
        };
        entry.rerun_nested(exec, binding, members, &mut run)
    }
    /// D4120: a stale set's members run again with its kept members filled again (`Entry::refill_nested`), as `Leader`.
    fn refill_members(&mut self, members: &[u16]) -> gravity_driver::Result<()> {
        // SAFETY: as `run_members`.
        let (entry, exec, binding) = unsafe { (&mut *self.entry, &mut *self.exec, &mut *self.binding) };
        let out_rows = self.out_rows;
        let mut run = |e: &mut Entry, x: &mut Exec, bd: &mut Binding<'_>, p: usize| -> gravity_driver::Result<()> {
            let mut call = Call::batch(out_rows);
            loop {
                match e.run_pipeline(x, bd, call)? {
                    Step::Done => return super::drive::finish(e, x, bd, p),
                    Step::Suspended { .. } => call = Call::resume(out_rows),
                    s => return Err(gravity_driver::Error::host(format!("v4 rig: refill member {p} stopped on {s:?}"))),
                }
            }
        };
        entry.refill_nested(exec, binding, members, &mut run)
    }
    /// M8 RT-2f (nest.cte CT-2): a pulled set's members run to the budget stop or the end (`Entry::pull_nested`), as `Leader`.
    fn pull_members(&mut self, members: &[u16], _budget: u64) -> gravity_driver::Result<gravity_driver::Pulled> {
        // SAFETY: as `run_members`.
        let (entry, exec, binding) = unsafe { (&mut *self.entry, &mut *self.exec, &mut *self.binding) };
        let mut run = |e: &mut Entry, x: &mut Exec, bd: &mut Binding<'_>, p: usize| -> gravity_driver::Result<()> {
            let mut call = Call::batch(self.out_rows);
            loop {
                match e.run_pipeline(x, bd, call)? {
                    Step::Done => return super::drive::finish(e, x, bd, p),
                    Step::Suspended { .. } => call = Call::resume(self.out_rows),
                    s => return Err(gravity_driver::Error::host(format!("v4 rig: pulled member {p} stopped on {s:?}"))),
                }
            }
        };
        entry.pull_nested(exec, binding, members, &mut run)
    }
}

/// The rig's frame published to each Init live while this lives (gravity_parallel's `publish`, for the rig).
struct Published<'l> {
    lives: &'l Lives,
}

impl<'l> Published<'l> {
    fn new<'f>(lives: &'l Lives, f: &'f mut (dyn OnDemand + 'f)) -> Self {
        // SAFETY: the lifetime is erased only for the lives' cells, which `drop` clears before `f` goes (`run` drops this
        // before its frame, an unwind included).
        let frame = unsafe { core::mem::transmute::<core::ptr::NonNull<dyn OnDemand + 'f>, gravity_driver::Frame>(core::ptr::NonNull::from(f)) };
        lives.iter().for_each(|l| l.frame(Some(frame)));
        Published { lives }
    }
}

impl Drop for Published<'_> {
    fn drop(&mut self) {
        self.lives.iter().for_each(|l| l.frame(None));
    }
}

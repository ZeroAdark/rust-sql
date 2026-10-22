use init_small::globals as g;
use types_error::{PgError, PgResult, LOG};
use types_startup::StartupData;
use types_storage::waiteventset::{WL_EXIT_ON_PM_DEATH, WL_LATCH_SET, WL_TIMEOUT};

use crate::apply::Pending;
use crate::config::ConfigState;
use crate::http::Response;
use crate::liveness::Liveness;
use crate::sample::Sampler;
use crate::shared::{now_ms, Job, JobKind, SHARED};

const PG_WAIT_EXTENSION: u32 = 0x0700_0000;
const TICK_MS: i64 = 100;
const SAMPLE_MS: i64 = 250;

enum Running {
    Apply(Pending),
    Promote(i64),
    Roles(bgworker::BackgroundWorkerHandle, i64),
}

struct Aux {
    sampler: Sampler,
    live: Liveness,
    config: ConfigState,
    reload_gen: u64,
    job: Option<(u64, Running)>,
    next_sample_ms: i64,
}

fn fatal_exit(e: &PgError) -> ! {
    elog::emit_error_report_for(e);
    ipc::proc_exit(1, g::MyProcPid())
}

pub fn InstanceAuxMain(startup_data: &StartupData) -> ! {
    debug_assert!(matches!(startup_data, StartupData::None));
    miscinit::SetMyBackendType(types_core::BackendType::InstanceAux);
    if let Err(e) = auxprocess::AuxiliaryProcessMainCommon() {
        fatal_exit(&e);
    }
    {
        use procsignal::ThreadSignalHandler::{Ignore, Simple};
        use procsignal::signums as s;
        procsignal::pqsignal_thread(s::SIGHUP, Simple(interrupt::SignalHandlerForConfigReload));
        procsignal::pqsignal_thread(s::SIGINT, Simple(interrupt::SignalHandlerForShutdownRequest));
        procsignal::pqsignal_thread(s::SIGTERM, Simple(interrupt::SignalHandlerForShutdownRequest));
        procsignal::pqsignal_thread(s::SIGALRM, Ignore);
        procsignal::pqsignal_thread(s::SIGPIPE, Ignore);
        procsignal::pqsignal_thread(s::SIGUSR2, Ignore);
    }
    libpq_pqsignal::unblock_signals();
    abandon_orphan_job();
    let in_recovery = transam_xlog::RecoveryInProgress();
    let mut aux = Aux {
        sampler: Sampler::new(),
        live: Liveness::new(),
        config: crate::config::current_state(in_recovery),
        reload_gen: 0,
        job: None,
        next_sample_ms: 0,
    };
    loop {
        if let Err(e) = aux.cycle() {
            aux.recover(&e);
        }
        if let Err(e) = latch::WaitLatch(g::MyLatch(), WL_LATCH_SET | WL_TIMEOUT | WL_EXIT_ON_PM_DEATH, TICK_MS, PG_WAIT_EXTENSION) {
            fatal_exit(&e);
        }
    }
}

fn abandon_orphan_job() {
    let mut slot = pgsync::lock(&SHARED.jobs);
    if let Some(id) = slot.running.take() {
        slot.done = Some((id, Response::error(503, "aux_restarted", "the instance aux restarted while running the request")));
        SHARED.jobs_cv.notify_all();
    }
}

fn take_job() -> Option<(u64, Job)> {
    let mut slot = pgsync::lock(&SHARED.jobs);
    let (id, job) = slot.pending.take()?;
    slot.running = Some(id);
    Some((id, job))
}

fn finish(id: u64, r: Response) {
    let mut slot = pgsync::lock(&SHARED.jobs);
    if slot.running == Some(id) {
        slot.running = None;
    }
    slot.done = Some((id, r));
    SHARED.jobs_cv.notify_all();
}

impl Aux {
    fn cycle(&mut self) -> PgResult<()> {
        if let Some(l) = g::MyLatch() {
            latch::ResetLatch(l);
        }
        let reload = interrupt::ConfigReloadPending();
        interrupt::ProcessMainLoopInterrupts()?;
        if reload {
            self.reload_gen += 1;
            self.config = crate::config::current_state(transam_xlog::RecoveryInProgress());
        }
        self.advance_job()?;
        if self.job.is_none() {
            if let Some((id, job)) = take_job() {
                self.start_job(id, job)?;
            }
        }
        let now = now_ms();
        if !transam_xlog::RecoveryInProgress() && self.live.due(now) {
            self.live.commit()?;
        }
        if now >= self.next_sample_ms {
            crate::shared::publish_sample(self.sampler.take(&self.live, &self.config));
            self.next_sample_ms = now + SAMPLE_MS;
        }
        Ok(())
    }

    fn recover(&mut self, e: &PgError) {
        g::SetInterruptHoldoffCount(0);
        g::SetCritSectionCount(0);
        g::HoldInterrupts();
        elog::emit_error_report_for(e);
        let _ = lwlock::LWLockReleaseAll();
        let _ = resowner::ReleaseAuxProcessResources(false);
        let _ = fd::AtEOXact_Files(false);
        elog::FlushErrorState();
        g::ResumeInterrupts();
        SHARED.commit_started_ms.store(0, std::sync::atomic::Ordering::Relaxed);
        if let Some((id, _)) = self.job.take() {
            finish(id, Response::error(500, "internal", e.message()));
        }
    }

    fn start_job(&mut self, id: u64, job: Job) -> PgResult<()> {
        let _ = elog::elog(LOG, format!("instance aux: {:?} requested by \"{}\"", job.kind, job.caller));
        if job.kind == JobKind::ConfigCheck {
            let r = match crate::config::parse_request(&job.body) {
                Ok(req) => crate::config::check_response(&crate::config::check(&req, transam_xlog::RecoveryInProgress())),
                Err(r) => r,
            };
            finish(id, r);
            return Ok(());
        }
        let v = match crate::admit::body_json(&job.body).and_then(|v| crate::admit::admit(&v).map(|_| v)) {
            Ok(v) => v,
            Err(r) => {
                finish(id, r);
                return Ok(());
            }
        };
        match job.kind {
            JobKind::ConfigCheck => {}
            JobKind::ConfigApply => match crate::apply::begin(&job.body, self.reload_gen)? {
                Ok(p) => self.job = Some((id, Running::Apply(p))),
                Err(r) => finish(id, r),
            },
            JobKind::Promote => match crate::promote::begin(&v)? {
                Ok(deadline) => self.job = Some((id, Running::Promote(deadline))),
                Err(r) => finish(id, r),
            },
            JobKind::Fence => finish(id, crate::fence::run(&v, &job.caller)?),
            JobKind::Roles => {
                if let Some(r) = crate::roles::refuse_in_recovery() {
                    finish(id, r);
                    return Ok(());
                }
                match crate::roles::parse_request(&job.body) {
                    Err(r) => finish(id, r),
                    Ok(work) => match crate::roles::begin(work)? {
                        Some(h) => self.job = Some((id, Running::Roles(h, now_ms() + 90_000))),
                        None => finish(id, Response::error(503, "no_worker_slot", "no background worker slot is free")),
                    },
                }
            }
            JobKind::TlsReload => finish(id, crate::tls::reload_response()),
        }
        Ok(())
    }

    fn advance_job(&mut self) -> PgResult<()> {
        let Some((id, running)) = &self.job else { return Ok(()) };
        let id = *id;
        let done = match running {
            Running::Apply(p) => crate::apply::poll(p, self.reload_gen, &self.config),
            Running::Promote(deadline) => crate::promote::poll(*deadline),
            Running::Roles(h, deadline) => match bgworker::GetBackgroundWorkerPid(h).0 {
                bgworker::BgwHandleStatus::BGWH_STOPPED | bgworker::BgwHandleStatus::BGWH_POSTMASTER_DIED => {
                    Some(match crate::roles::take_result() {
                        Some(r) => crate::roles::outcome_response(r),
                        None => Response::error(500, "role_worker_failed", "the role apply worker exited without a result"),
                    })
                }
                _ if now_ms() > *deadline => {
                    bgworker::TerminateBackgroundWorker(h);
                    Some(Response::error(504, "timeout", "the role apply worker did not finish in time"))
                }
                _ => None,
            },
        };
        if let Some(r) = done {
            self.job = None;
            finish(id, r);
        }
        Ok(())
    }
}

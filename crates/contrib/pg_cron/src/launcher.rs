//! The pg_cron scheduler background worker (C: pg_cron.c PgCronLauncherMain,

use std::collections::BTreeMap;

use types_error::{ErrorLevel, PgResult, FATAL, LOG, WARNING};
use types_storage::waiteventset::{WL_EXIT_ON_PM_DEATH, WL_LATCH_SET, WL_TIMEOUT};

use crate::joblist::{self, CronJob};
use crate::rundetails::{self, CronStatus, TimestampTz};
use crate::schedule::{Entry, LocalTime, MINUTE_COUNT, WHEN_REBOOT};
use crate::task::{self, CronTask, JobSpec, RunningJob, TaskState, TaskTable};
use crate::{gucs, worker};

const PG_WAIT_EXTENSION: u32 = 0x0700_0000;
/// `MaxWait`: the longest one tick may block (ms).
const MAX_WAIT_MS: i64 = 1000;
/// Tick cap while jobs are running: C wakes on the worker's shm_mq latch
const RUNNING_POLL_MS: i64 = 100;
/// `CronTaskStartTimeout`: how long to keep retrying a worker slot (ms).
const CRON_TASK_START_TIMEOUT_MS: i64 = 10_000;
const USECS_PER_MINUTE: i64 = 60_000_000;
const USECS_PER_DAY: i64 = 86_400_000_000;

/// `ClockProgress` (pg_cron.h).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ClockProgress {
    Progressed,
    JumpForward,
    JumpBackward,
    Change,
}

#[track_caller]
fn loc(func: &'static str) -> types_error::ErrorLocation {
    let site = core::panic::Location::caller();
    types_error::ErrorLocation::new(site.file(), site.line() as i32, func)
}

/// Static registration (pg_cron.c `_PG_init` tail): called by pg_init while
pub fn register() -> PgResult<()> {
    let bgw = bgworker::BackgroundWorker {
        bgw_name: "pg_cron launcher".to_string(),
        bgw_type: "pg_cron launcher".to_string(),
        bgw_flags: bgworker::BGWORKER_SHMEM_ACCESS | bgworker::BGWORKER_BACKEND_DATABASE_CONNECTION,
        bgw_start_time: bgworker::BgWorkerStartTime::RecoveryFinished,
        bgw_restart_time: 1,
        bgw_main: launcher_main_thunk,
        bgw_main_arg: 0,
        bgw_extra: [0; bgworker::BGW_EXTRALEN],
        bgw_notify_pid: 0,
    };
    bgworker::RegisterBackgroundWorker(&bgw)
}

fn launcher_main_thunk(main_arg: u64) -> PgResult<()> {
    Launcher::run(main_arg)
}

/// `cron_error_severity`: the GUC spelling of a message level.
fn error_severity_name(level: i32) -> &'static str {
    match ErrorLevel(level) {
        types_error::DEBUG1 => "DEBUG1",
        types_error::DEBUG2 => "DEBUG2",
        types_error::DEBUG3 => "DEBUG3",
        types_error::DEBUG4 => "DEBUG4",
        types_error::DEBUG5 => "DEBUG5",
        types_error::LOG => "LOG",
        types_error::INFO => "INFO",
        types_error::NOTICE => "NOTICE",
        types_error::WARNING => "WARNING",
        types_error::ERROR => "ERROR",
        types_error::FATAL => "FATAL",
        types_error::PANIC => "PANIC",
        _ => "???",
    }
}

fn apply_log_min_messages() -> PgResult<()> {
    guc::SetConfigOption(
        "log_min_messages",
        Some(error_severity_name(gucs::log_min_messages())),
        types_guc::GucContext::PGC_POSTMASTER,
        types_guc::GucSource::PGC_S_OVERRIDE,
    )
}

/// Non-FATAL errors from catalog work are contained: abort the open
fn contain(e: Box<types_error::PgError>, what: &str) -> PgResult<()> {
    if e.level() >= FATAL {
        return Err(e);
    }
    if xact::IsTransactionState() {
        let _ = xact::AbortCurrentTransaction();
    }
    elog::emit_error_report_for(&e);
    let _ = elog::elog(WARNING, format!("pg_cron launcher: error while {what}: {}", e.message()));
    Ok(())
}

/// `TimestampMinuteStart`.
fn minute_start(t: TimestampTz) -> TimestampTz {
    t - t.rem_euclid(USECS_PER_MINUTE)
}

/// `TimestampMinuteEnd`.
fn minute_end(t: TimestampTz) -> TimestampTz {
    minute_start(t) + USECS_PER_MINUTE
}

/// `MinutesPassed`: whole minutes from start to stop (negative on a
fn minutes_passed(start: TimestampTz, stop: TimestampTz) -> i64 {
    let (secs, _) = adt_timestamp::TimestampDifference(start, stop);
    secs / 60
}

fn plus_ms(t: TimestampTz, ms: i64) -> TimestampTz {
    t + ms * 1000
}

struct Launcher {
    tz: localtime::PgTzRef,
    jobs: BTreeMap<i64, CronJob>,
    tasks: TaskTable,
    running_task_count: i32,
    max_running_tasks: i32,
    reboot_jobs_scheduled: bool,
    last_minute: TimestampTz,
}

impl Launcher {
    fn run(_main_arg: u64) -> PgResult<()> {
        procsignal::pqsignal_thread(
            procsignal::signums::SIGHUP,
            procsignal::ThreadSignalHandler::Simple(interrupt::SignalHandlerForConfigReload),
        );
        bgworker::BackgroundWorkerUnblockSignals();

        let dbname = gucs::database_name();
        bgworker::BackgroundWorkerInitializeConnection(Some(&dbname), None, 0)?;
        backend_status::pgstat_report_appname("pg_cron scheduler");
        joblist::register_invalidation_callback()?;

        if let Err(e) = rundetails::mark_pending_runs_as_failed() {
            contain(e, "marking pending runs as failed")?;
        }

        let mut max_running = gucs::max_running_jobs();
        max_running = max_running.min(guc_tables::vars::MaxConnections.read());
        max_running = max_running.min(guc_tables::vars::max_files_per_process.read());
        max_running = max_running.min(guc_tables::vars::max_worker_processes.read() - 1);
        if max_running <= 0 {
            max_running = 1;
        }

        let tz = match pgtz::pg_tzset(gucs::timezone().as_bytes())? {
            Some(tz) => tz,
            None => pgtz::pg_tzset(b"GMT")?.expect("GMT is always a valid zone"),
        };

        let _ = elog::elog(LOG, "pg_cron scheduler started");
        apply_log_min_messages()?;

        let mut me = Launcher {
            tz,
            jobs: BTreeMap::new(),
            tasks: TaskTable::new(),
            running_task_count: 0,
            max_running_tasks: max_running,
            reboot_jobs_scheduled: false,
            last_minute: 0,
        };
        me.main_loop()
    }

    fn main_loop(&mut self) -> PgResult<()> {
        loop {
            postgres_seams::check_for_interrupts::call()?;
            if let Err(e) = inval::local::AcceptInvalidationMessages() {
                contain(e, "accepting invalidation messages")?;
            }

            if interrupt::ConfigReloadPending() {
                interrupt::SetConfigReloadPending(false);
                if let Err(e) = guc_file::ProcessConfigFile(types_guc::GucContext::PGC_SIGHUP) {
                    contain(e, "reloading configuration")?;
                }
                if let Err(e) = apply_log_min_messages() {
                    contain(e, "applying cron.log_min_messages")?;
                }
                joblist::set_cache_valid(false);
            }

            if !joblist::cache_valid() {
                match joblist::load_cron_job_list() {
                    Ok(jobs) => {
                        let now = adt_timestamp::GetCurrentTimestamp();
                        self.tasks.refresh(&jobs, now, gucs::launch_active_jobs());
                        self.jobs = jobs.into_iter().map(|j| (j.job_id, j)).collect();
                        joblist::set_cache_valid(true);
                    }
                    Err(e) => contain(e, "loading the job list")?,
                }
            }

            let now = adt_timestamp::GetCurrentTimestamp();
            self.start_all_pending_runs(now);
            self.wait_for_cron_tasks(now)?;
            self.manage_cron_tasks(now)?;
        }
    }

    /// `ShouldRunTask`'s clock half: break `t` down in cron.timezone and ask
    fn should_run(&self, schedule: &Entry, t: TimestampTz, do_wild: bool, do_nonwild: bool) -> bool {
        let secs = adt_timestamp::timestamptz_to_time_t(t);
        let tomorrow = adt_timestamp::timestamptz_to_time_t(t + USECS_PER_DAY);
        let Some(tm) = adt_datetime::tz::pg_localtime(secs, &self.tz) else { return false };
        let is_lastdom = adt_datetime::tz::pg_localtime(tomorrow, &self.tz).is_some_and(|tt| tt.tm_mday == 1);
        let now = LocalTime {
            minute: tm.tm_min,
            hour: tm.tm_hour,
            day_of_month: tm.tm_mday,
            month: tm.tm_mon,
            day_of_week: tm.tm_wday,
        };
        schedule.matches(now, is_lastdom, gucs::match_dom_and_dow(), do_wild, do_nonwild)
    }

    /// `StartAllPendingRuns`.
    fn start_all_pending_runs(&mut self, current_time: TimestampTz) {
        if !self.reboot_jobs_scheduled {
            for task in self.tasks.tasks.values_mut() {
                let at_restart = self.jobs.get(&task.job_id).is_some_and(|j| j.schedule.flags & WHEN_REBOOT != 0);
                if at_restart && task.is_active {
                    task.pending_run_count += 1;
                }
            }
            self.reboot_jobs_scheduled = true;
        }

        for task in self.tasks.tasks.values_mut() {
            if task.seconds_interval > 0
                && task.is_active
                && task.pending_run_count == 0
                && adt_timestamp::TimestampDifferenceExceeds(
                    task.last_start_time,
                    current_time,
                    (task.seconds_interval * 1000) as i32,
                )
            {
                task.pending_run_count += 1;
            }
        }

        if self.last_minute == 0 {
            self.last_minute = minute_start(current_time);
        }
        let minutes = minutes_passed(self.last_minute, current_time);
        if minutes == 0 {
            return;
        }
        let progress = if minutes > 3 * MINUTE_COUNT {
            ClockProgress::Change
        } else if minutes > 5 {
            ClockProgress::JumpForward
        } else if minutes > 0 {
            ClockProgress::Progressed
        } else if minutes > -(3 * MINUTE_COUNT) {
            ClockProgress::JumpBackward
        } else {
            ClockProgress::Change
        };

        let last_minute = self.last_minute;
        let ids = self.tasks.job_ids();
        for id in ids {
            let (active, schedule) = match (self.tasks.tasks.get(&id), self.jobs.get(&id)) {
                (Some(t), Some(j)) => (t.is_active, j.schedule.clone()),
                _ => continue,
            };
            if !active {
                continue;
            }
            let added = self.pending_runs_for(&schedule, progress, last_minute, current_time);
            if let Some(t) = self.tasks.tasks.get_mut(&id) {
                t.pending_run_count += added;
            }
        }

        if progress != ClockProgress::JumpBackward {
            self.last_minute = minute_start(current_time);
        }
    }

    /// `StartPendingRuns`: how many runs `schedule` earns for the minutes
    fn pending_runs_for(
        &self,
        schedule: &Entry,
        progress: ClockProgress,
        last_minute: TimestampTz,
        current_time: TimestampTz,
    ) -> u32 {
        let current_minute = minute_start(current_time);
        let mut virtual_time = last_minute;
        let mut runs = 0;
        match progress {
            ClockProgress::Progressed => loop {
                virtual_time = plus_ms(virtual_time, 60 * 1000);
                if self.should_run(schedule, virtual_time, true, true) {
                    runs += 1;
                }
                if virtual_time >= current_minute {
                    break;
                }
            },
            ClockProgress::JumpForward => {
                loop {
                    virtual_time = plus_ms(virtual_time, 60 * 1000);
                    if self.should_run(schedule, virtual_time, false, true) {
                        runs += 1;
                    }
                    if virtual_time >= current_minute {
                        break;
                    }
                }
                if self.should_run(schedule, current_minute, true, false) {
                    runs += 1;
                }
            }
            ClockProgress::JumpBackward => {
                if self.should_run(schedule, current_minute, true, false) {
                    runs += 1;
                }
            }
            ClockProgress::Change => {
                if self.should_run(schedule, current_minute, true, true) {
                    runs += 1;
                }
            }
        }
        runs
    }

    /// `WaitForCronTasks` + PollForTasks' timeout: sleep until the next
    fn wait_for_cron_tasks(&self, current_time: TimestampTz) -> PgResult<()> {
        let mut next_event = minute_end(current_time);
        let mut any_running = false;
        for task in self.tasks.tasks.values() {
            if task.state == TaskState::Done || task.state == TaskState::Error || self.can_start(task) {
                return Ok(());
            }
            if task.state == TaskState::Waiting && task.pending_run_count == 0 && task.seconds_interval > 0 {
                let next_run = plus_ms(task.last_start_time, task.seconds_interval as i64 * 1000);
                if next_run < next_event {
                    next_event = next_run;
                }
            }
            if task.state == TaskState::BgwRunning {
                any_running = true;
            }
        }
        let mut timeout_ms = (next_event - current_time) / 1000;
        if timeout_ms <= 0 {
            timeout_ms = 1;
        } else if timeout_ms > MAX_WAIT_MS {
            timeout_ms = MAX_WAIT_MS;
        }
        if any_running {
            timeout_ms = timeout_ms.min(RUNNING_POLL_MS);
        }
        let rc = latch::WaitLatch(
            init_small::globals::MyLatch(),
            WL_LATCH_SET | WL_TIMEOUT | WL_EXIT_ON_PM_DEATH,
            timeout_ms,
            PG_WAIT_EXTENSION,
        )?;
        if rc & WL_LATCH_SET != 0 {
            if let Some(l) = init_small::globals::MyLatch() {
                latch::ResetLatch(l);
            }
        }
        postgres_seams::check_for_interrupts::call()
    }

    fn can_start(&self, task: &CronTask) -> bool {
        task.state == TaskState::Waiting
            && task.pending_run_count > 0
            && self.running_task_count < self.max_running_tasks
    }

    /// `ManageCronTasks`.
    fn manage_cron_tasks(&mut self, current_time: TimestampTz) -> PgResult<()> {
        for id in self.tasks.job_ids() {
            if let Err(e) = self.manage_cron_task(id, current_time) {
                contain(e, &format!("managing cron job {id}"))?;
            }
        }
        Ok(())
    }

    /// `ManageCronTask`: one pass over the state machine for job `id`.
    fn manage_cron_task(&mut self, id: i64, current_time: TimestampTz) -> PgResult<()> {
        let Some(state) = self.tasks.tasks.get(&id).map(|t| t.state) else { return Ok(()) };
        let mut state = state;

        if state == TaskState::Waiting {
            let task = self.tasks.tasks.get_mut(&id).expect("task present");
            if !task.is_active {
                self.tasks.remove(id);
                return Ok(());
            }
            if !self.can_start(self.tasks.tasks.get(&id).expect("task present")) {
                return Ok(());
            }
            let task = self.tasks.tasks.get_mut(&id).expect("task present");
            task.pending_run_count -= 1;
            task.state = TaskState::BgwStart;
            task.last_start_time = current_time;
            self.running_task_count += 1;
            let run_id = rundetails::next_run_id()?;
            let task = self.tasks.tasks.get_mut(&id).expect("task present");
            task.run_id = run_id;
            if gucs::log_run() {
                if let Some(job) = self.jobs.get(&id) {
                    rundetails::insert_job_run_detail(
                        run_id,
                        job.job_id,
                        &job.database,
                        &job.user_name,
                        &job.command,
                        CronStatus::Starting,
                    )?;
                }
            }
            state = TaskState::BgwStart;
        }

        if state == TaskState::BgwStart {
            self.start_worker(id, current_time)?;
            return Ok(());
        }

        if state == TaskState::BgwRunning {
            let task = self.tasks.tasks.get_mut(&id).expect("task present");
            if !task.is_active {
                task.error_message = Some("job canceled".to_string());
                task.state = TaskState::Error;
                if let Some(run) = task.run.take() {
                    bgworker::TerminateBackgroundWorker(&run.handle);
                    let _ = bgworker::WaitForBackgroundWorkerShutdown(&run.handle);
                    task::retire_job(run.key);
                }
                return Ok(());
            }
            let Some(run) = task.run.as_ref() else {
                task.state = TaskState::Error;
                task.error_message = Some("job worker state lost".to_string());
                return Ok(());
            };
            let (status, _pid) = bgworker::GetBackgroundWorkerPid(&run.handle);
            if status != bgworker::BgwHandleStatus::BGWH_STOPPED {
                return Ok(());
            }
            let run = task.run.take().expect("checked above");
            let run_id = task.run_id;
            let outcome = {
                let r = run.result.lock().unwrap_or_else(|e| e.into_inner());
                (r.error.clone(), r.command_tag.clone(), r.finished)
            };
            task::retire_job(run.key);
            task.state = TaskState::Done;
            self.running_task_count -= 1;
            let end_time = adt_timestamp::GetCurrentTimestamp();
            match outcome {
                (Some(msg), _, _) => {
                    if gucs::log_run() {
                        rundetails::update_job_run_detail(run_id, None, Some(CronStatus::Failed), Some(&msg), None, Some(end_time))?;
                    }
                }
                (None, Some(tag), _) => {
                    if gucs::log_run() {
                        rundetails::update_job_run_detail(run_id, None, Some(CronStatus::Succeeded), Some(&tag), None, Some(end_time))?;
                    }
                    if gucs::log_statement() {
                        let _ = elog::elog(LOG, format!("cron job {id} COMMAND completed: {tag}"));
                    }
                }
                (None, None, _) => {
                    if gucs::log_run() {
                        rundetails::update_job_run_detail(
                            run_id,
                            None,
                            Some(CronStatus::Failed),
                            Some("background worker exited without reporting a result"),
                            None,
                            Some(end_time),
                        )?;
                    }
                }
            }
            state = TaskState::Done;
        }

        if state == TaskState::Error {
            let task = self.tasks.tasks.get_mut(&id).expect("task present");
            let remove_after = !task.is_active;
            let run_id = task.run_id;
            let last_start = task.last_start_time;
            match task.error_message.take() {
                Some(msg) => {
                    if gucs::log_run() {
                        let end_time = adt_timestamp::GetCurrentTimestamp();
                        rundetails::update_job_run_detail(run_id, None, Some(CronStatus::Failed), Some(&msg), Some(last_start), Some(end_time))?;
                    }
                    let _ = elog::elog(LOG, format!("cron job {id} {msg}"));
                }
                None => {
                    let _ = elog::elog(LOG, format!("cron job {id} {}", CronStatus::Failed.as_str()));
                }
            }
            let task = self.tasks.tasks.get_mut(&id).expect("task present");
            task.start_deadline = 0;
            task.state = TaskState::Done;
            self.running_task_count -= 1;
            if remove_after {
                self.tasks.remove(id);
                return Ok(());
            }
            state = TaskState::Done;
        }

        if state == TaskState::Done {
            let job_active = self.jobs.get(&id).is_some_and(|j| j.active);
            let task = self.tasks.tasks.get_mut(&id).expect("task present");
            let pending = task.pending_run_count;
            if job_active {
                let (interval, active, last_start) = (task.seconds_interval, task.is_active, task.last_start_time);
                task.reinitialize();
                task.seconds_interval = interval;
                task.is_active = active;
                task.last_start_time = last_start;
            } else {
                task.state = TaskState::Waiting;
            }
            task.pending_run_count = pending;
        }
        Ok(())
    }

    /// The BGW_START arm: publish the job, register a dynamic worker
    fn start_worker(&mut self, id: i64, current_time: TimestampTz) -> PgResult<()> {
        let Some(job) = self.jobs.get(&id).cloned() else {
            if let Some(task) = self.tasks.tasks.get_mut(&id) {
                task.error_message = Some("job canceled".to_string());
                task.state = TaskState::Error;
            }
            return Ok(());
        };
        let run_id = self.tasks.tasks.get(&id).map_or(0, |t| t.run_id);
        if gucs::log_statement() {
            let _ = elog::elog(
                LOG,
                format!("cron job {id} {}: {}", CronStatus::Starting.as_str(), job.command),
            );
        }
        let (key, result) = task::publish_job(JobSpec {
            job_id: job.job_id,
            run_id,
            database: job.database.clone(),
            username: job.user_name.clone(),
            command: job.command.clone(),
        });
        let worker = bgworker::BackgroundWorker {
            bgw_name: "pg_cron worker".to_string(),
            bgw_type: "pg_cron".to_string(),
            bgw_flags: bgworker::BGWORKER_SHMEM_ACCESS | bgworker::BGWORKER_BACKEND_DATABASE_CONNECTION,
            bgw_start_time: bgworker::BgWorkerStartTime::ConsistentState,
            bgw_restart_time: bgworker::BGW_NEVER_RESTART,
            bgw_main: worker::cron_background_worker_main,
            bgw_main_arg: key,
            bgw_extra: [0; bgworker::BGW_EXTRALEN],
            bgw_notify_pid: init_small::globals::MyProcPid(),
        };

        let start_deadline = plus_ms(current_time, CRON_TASK_START_TIMEOUT_MS);
        if let Some(task) = self.tasks.tasks.get_mut(&id) {
            task.start_deadline = start_deadline;
        }
        let handle = loop {
            match bgworker::RegisterDynamicBackgroundWorker(worker.clone())? {
                Some(h) => break Some(h),
                None => {
                    if adt_timestamp::TimestampDifferenceExceeds(start_deadline, adt_timestamp::GetCurrentTimestamp(), 0) {
                        break None;
                    }
                    latch::WaitLatch(
                        init_small::globals::MyLatch(),
                        WL_LATCH_SET | WL_TIMEOUT | WL_EXIT_ON_PM_DEATH,
                        10,
                        PG_WAIT_EXTENSION,
                    )?;
                    if let Some(l) = init_small::globals::MyLatch() {
                        latch::ResetLatch(l);
                    }
                    postgres_seams::check_for_interrupts::call()?;
                }
            }
        };
        let fail = |this: &mut Launcher, msg: &str| {
            task::retire_job(key);
            if let Some(task) = this.tasks.tasks.get_mut(&id) {
                task.state = TaskState::Error;
                task.error_message = Some(msg.to_string());
            }
        };
        let Some(handle) = handle else {
            fail(self, "could not start background process; more details may be available in the server log");
            let _ = elog::ereport(WARNING)
                .errcode(types_error::ERRCODE_CONFIGURATION_LIMIT_EXCEEDED)
                .errmsg("out of background worker slots")
                .errhint("You might need to increase max_worker_processes.")
                .finish(loc("start_worker"));
            return Ok(());
        };
        if let Some(task) = self.tasks.tasks.get_mut(&id) {
            task.start_deadline = 0;
        }
        let (status, pid) = bgworker::WaitForBackgroundWorkerStartup(&handle)?;
        if status != bgworker::BgwHandleStatus::BGWH_STARTED && status != bgworker::BgwHandleStatus::BGWH_STOPPED {
            fail(self, "could not start background process; more details may be available in the server log");
            return Ok(());
        }
        let started_at = adt_timestamp::GetCurrentTimestamp();
        if let Some(task) = self.tasks.tasks.get_mut(&id) {
            task.last_start_time = started_at;
            task.state = TaskState::BgwRunning;
            task.run = Some(RunningJob { handle, key, result });
        }
        if gucs::log_run() {
            rundetails::update_job_run_detail(run_id, Some(pid), Some(CronStatus::Running), None, Some(started_at), None)?;
        }
        Ok(())
    }
}

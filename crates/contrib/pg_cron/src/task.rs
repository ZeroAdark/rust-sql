//! Per-job task state (C: task_states.h CronTask/CronTaskState,

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use pgsync::Mutex;

use crate::joblist::CronJob;
use crate::rundetails::TimestampTz;

/// `CronTaskState`, background-worker arms only (the libpq CONNECTING /
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskState {
    Waiting,
    BgwStart,
    BgwRunning,
    Done,
    Error,
}

/// `CronTask`.
#[derive(Debug)]
pub struct CronTask {
    pub job_id: i64,
    pub run_id: i64,
    pub state: TaskState,
    pub pending_run_count: u32,
    pub start_deadline: TimestampTz,
    pub last_start_time: TimestampTz,
    pub seconds_interval: u32,
    pub is_active: bool,
    pub error_message: Option<String>,
    /// The running job's worker handle + result slot (BGW_RUNNING only).
    pub run: Option<RunningJob>,
}

#[derive(Debug)]
pub struct RunningJob {
    pub handle: bgworker::BackgroundWorkerHandle,
    pub key: u64,
    pub result: Arc<Mutex<JobResult>>,
}

impl CronTask {
    /// `InitializeCronTask`.
    pub fn new(job_id: i64) -> Self {
        CronTask {
            job_id,
            run_id: 0,
            state: TaskState::Waiting,
            pending_run_count: 0,
            start_deadline: 0,
            last_start_time: 0,
            seconds_interval: 0,
            is_active: true,
            error_message: None,
            run: None,
        }
    }

    /// `InitializeCronTask` on an existing task (the DONE arm): reset every
    pub fn reinitialize(&mut self) {
        let job_id = self.job_id;
        *self = CronTask::new(job_id);
    }
}

/// What a job worker hands back (C: the 'C' CommandComplete / 'E' error
#[derive(Debug, Default)]
pub struct JobResult {
    /// Set once the worker has connected and started running.
    pub started: bool,
    /// Completion tag of the last statement ("SELECT 1", "VACUUM"), Ok arm.
    pub command_tag: Option<String>,
    /// `bgw_generate_returned_message` text, Err arm.
    pub error: Option<String>,
    pub finished: bool,
}

/// `JobSpec`: what CronBackgroundWorker reads out of the shm_toc.
#[derive(Debug, Clone)]
pub struct JobSpec {
    pub job_id: i64,
    pub run_id: i64,
    pub database: String,
    pub username: String,
    pub command: String,
}

pgsync::process_global! {
    static JOB_REGISTRY: Mutex<BTreeMap<u64, (JobSpec, Arc<Mutex<JobResult>>)>> =
        Mutex::new(BTreeMap::new());
}
static NEXT_KEY: AtomicU64 = AtomicU64::new(1);

/// Launcher side: publish a job for a worker; the key becomes bgw_main_arg.
pub fn publish_job(spec: JobSpec) -> (u64, Arc<Mutex<JobResult>>) {
    let key = NEXT_KEY.fetch_add(1, Ordering::Relaxed);
    let result = Arc::new(Mutex::new(JobResult::default()));
    JOB_REGISTRY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, (spec, Arc::clone(&result)));
    (key, result)
}

/// Worker side: look the job up by bgw_main_arg (dsm_attach + shm_toc_lookup).
pub fn lookup_job(key: u64) -> Option<(JobSpec, Arc<Mutex<JobResult>>)> {
    JOB_REGISTRY.lock().unwrap_or_else(|e| e.into_inner()).get(&key).cloned()
}

/// Launcher side, on task cleanup (dsm_detach): drop the registry entry.
pub fn retire_job(key: u64) {
    JOB_REGISTRY.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
}

/// The launcher's task table (C: CronTaskHash), keyed by jobid; BTreeMap so
pub struct TaskTable {
    pub tasks: BTreeMap<i64, CronTask>,
}

impl TaskTable {
    pub fn new() -> Self {
        TaskTable { tasks: BTreeMap::new() }
    }

    /// `RefreshTaskHash` (task_states.c): mark every task inactive, then
    pub fn refresh(&mut self, jobs: &[CronJob], now: TimestampTz, launch_active_jobs: bool) {
        for t in self.tasks.values_mut() {
            t.is_active = false;
        }
        for job in jobs {
            let task = self.tasks.entry(job.job_id).or_insert_with(|| {
                let mut t = CronTask::new(job.job_id);
                t.last_start_time = now;
                t
            });
            task.is_active = launch_active_jobs && job.active;
            task.seconds_interval = job.schedule.seconds_interval;
        }
    }

    /// `RemoveTask`.
    pub fn remove(&mut self, job_id: i64) {
        self.tasks.remove(&job_id);
    }

    pub fn job_ids(&self) -> Vec<i64> {
        self.tasks.keys().copied().collect()
    }
}

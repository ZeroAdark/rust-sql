use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicU8, Ordering::Relaxed};
use std::sync::Arc;

use openssl::ssl::SslAcceptor;
use pgsync::{Condvar, Mutex, RwLock};

use crate::http::Response;
use crate::sample::Sample;

#[cfg(test)]
pub(crate) const PM_STARTUP: u8 = 1;
pub(crate) const PM_HOT_STANDBY: u8 = 3;
pub(crate) const PM_RUN: u8 = 4;
pub(crate) const PM_STOP_BACKENDS: u8 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JobKind {
    ConfigCheck,
    ConfigApply,
    Promote,
    Fence,
    Roles,
    TlsReload,
}

pub(crate) struct Job {
    pub kind: JobKind,
    pub body: Vec<u8>,
    pub caller: String,
}

#[derive(Default)]
pub(crate) struct JobSlot {
    pub next_id: u64,
    pub pending: Option<(u64, Job)>,
    pub running: Option<u64>,
    pub done: Option<(u64, Response)>,
}

pub(crate) struct Shared {
    pub pm_state: AtomicU8,
    pub fatal_error: AtomicBool,
    pub pm_state_ms: AtomicI64,
    pub started_ms: AtomicI64,
    pub commit_started_ms: AtomicI64,
    pub commit_done_ms: AtomicI64,
    pub epoch: AtomicU64,
    pub fence_reason: Mutex<String>,
    pub sample: Mutex<Option<Arc<Sample>>>,
    pub acceptor: RwLock<Option<Arc<SslAcceptor>>>,
    pub jobs: Mutex<JobSlot>,
    pub jobs_cv: Condvar,
}

pub(crate) static SHARED: Shared = Shared {
    pm_state: AtomicU8::new(0),
    fatal_error: AtomicBool::new(false),
    pm_state_ms: AtomicI64::new(0),
    started_ms: AtomicI64::new(0),
    commit_started_ms: AtomicI64::new(0),
    commit_done_ms: AtomicI64::new(0),
    epoch: AtomicU64::new(0),
    fence_reason: Mutex::new(String::new()),
    sample: Mutex::new(None),
    acceptor: RwLock::new(None),
    jobs: Mutex::new(JobSlot { next_id: 1, pending: None, running: None, done: None }),
    jobs_cv: Condvar::new(),
};

pub(crate) fn now_ms() -> i64 {
    pg_clock::mono_ms()
}

/// Called by the postmaster on every PMState change; `state` is the PMState
/// discriminant (PM_STATE_NAMES order).
pub fn note_pm_state(state: u8, fatal_error: bool) {
    let s = &SHARED;
    if s.pm_state.swap(state, Relaxed) != state || s.fatal_error.swap(fatal_error, Relaxed) != fatal_error {
        s.pm_state_ms.store(now_ms(), Relaxed);
    }
}

pub(crate) fn latest_sample() -> Option<Arc<Sample>> {
    pgsync::lock(&SHARED.sample).clone()
}

pub(crate) fn publish_sample(s: Sample) {
    *pgsync::lock(&SHARED.sample) = Some(Arc::new(s));
}

pub(crate) fn acceptor() -> Option<Arc<SslAcceptor>> {
    SHARED.acceptor.read().unwrap_or_else(|e| e.into_inner()).clone()
}

pub(crate) fn install_acceptor(a: SslAcceptor) {
    *SHARED.acceptor.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(a));
}

use crate::sample::{Recovery, Sample};
use crate::shared::{PM_HOT_STANDBY, PM_RUN, PM_STOP_BACKENDS};

pub(crate) const SAMPLE_STALE_MS: i64 = 5_000;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Thresholds {
    pub ready_commit_age_ms: i64,
    pub ready_lag_ms: i64,
    pub startup_stall_ms: i64,
    pub wedge_ms: i64,
}

impl Thresholds {
    pub(crate) fn from_gucs() -> Self {
        use guc_tables::backing as b;
        Thresholds {
            ready_commit_age_ms: b::pgrust_instance_aux_ready_commit_age() as i64,
            ready_lag_ms: b::pgrust_instance_aux_ready_lag() as i64,
            startup_stall_ms: b::pgrust_instance_aux_startup_stall() as i64,
            wedge_ms: b::pgrust_instance_aux_wedge_timeout() as i64,
        }
    }
}

pub(crate) struct View<'a> {
    pub now_ms: i64,
    pub pm_state: u8,
    pub fatal_error: bool,
    pub pm_state_ms: i64,
    pub started_ms: i64,
    pub sample: Option<&'a Sample>,
    pub epoch: u64,
    pub fenced: bool,
    pub fence_reason: String,
    pub write_wedged: bool,
    pub limits: Thresholds,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Verdict {
    pub ok: bool,
    pub reason: Option<String>,
}

fn pass() -> Verdict {
    Verdict { ok: true, reason: None }
}

fn fail(reason: String) -> Verdict {
    Verdict { ok: false, reason: Some(reason) }
}

pub(crate) fn role(v: &View<'_>) -> &'static str {
    if v.fatal_error {
        return if v.pm_state >= PM_STOP_BACKENDS { "restarting" } else { "recovering" };
    }
    if v.pm_state >= PM_STOP_BACKENDS {
        return "stopping";
    }
    match v.sample.map(|s| s.recovery) {
        None => "starting",
        Some(Recovery::Crash) => "recovering",
        Some(Recovery::Archive) => "standby",
        Some(Recovery::Done) => "primary",
    }
}

fn accepting(v: &View<'_>) -> bool {
    !v.fatal_error && (v.pm_state == PM_RUN || v.pm_state == PM_HOT_STANDBY)
}

pub(crate) fn live(_v: &View<'_>) -> Verdict {
    pass()
}

pub(crate) fn startup(v: &View<'_>) -> Verdict {
    if accepting(v) {
        return pass();
    }
    if !v.fatal_error && v.pm_state >= PM_STOP_BACKENDS {
        return fail("the server is shutting down".into());
    }
    let progress = v.sample.map_or(0, |s| s.progress_ms).max(v.pm_state_ms).max(v.started_ms);
    let idle = v.now_ms - progress;
    if idle <= v.limits.startup_stall_ms {
        pass()
    } else {
        fail(format!("startup has made no progress for {idle} ms"))
    }
}

pub(crate) fn commit_age(v: &View<'_>) -> Option<i64> {
    v.sample.and_then(|s| s.live.acked_ms).map(|t| v.now_ms - t)
}

pub(crate) fn ready(v: &View<'_>) -> Verdict {
    if v.fenced {
        return fail("the instance is fenced".into());
    }
    if !accepting(v) {
        return fail(format!("the server is not accepting connections ({})", role(v)));
    }
    let Some(s) = v.sample else {
        return fail("the instance aux has not sampled the server yet".into());
    };
    let age = v.now_ms - s.taken_ms;
    if age > SAMPLE_STALE_MS {
        return fail(format!("the last instance aux sample is {age} ms old"));
    }
    match s.recovery {
        Recovery::Done => {
            if v.write_wedged {
                return fail("liveness commits are not completing (writes wedged)".into());
            }
            match commit_age(v) {
                None => fail("no liveness commit has been acknowledged yet".into()),
                Some(a) if a > v.limits.ready_commit_age_ms => {
                    fail(format!("the last acknowledged liveness commit is {a} ms old"))
                }
                Some(_) => pass(),
            }
        }
        Recovery::Archive => {
            if !s.hot_standby {
                return fail("hot standby is not active".into());
            }
            match s.lag_ms {
                Some(l) if l > v.limits.ready_lag_ms => fail(format!("the standby is {l} ms behind its WAL source")),
                _ => pass(),
            }
        }
        Recovery::Crash => fail("crash recovery is in progress".into()),
    }
}

use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use crate::http::{Request, Response};
use crate::probes::{self, Thresholds, View};
use crate::shared::{latest_sample, now_ms, Job, JobKind, SHARED};
use crate::status;

pub(crate) fn with_view<R>(f: impl FnOnce(&View<'_>) -> R) -> R {
    let sample = latest_sample();
    let now = now_ms();
    let limits = Thresholds::from_gucs();
    let external_wedged = sample.as_deref().is_some_and(|s| s.live.external_wedged);
    let v = View {
        now_ms: now,
        pm_state: SHARED.pm_state.load(Relaxed),
        fatal_error: SHARED.fatal_error.load(Relaxed),
        pm_state_ms: SHARED.pm_state_ms.load(Relaxed),
        started_ms: SHARED.started_ms.load(Relaxed),
        sample: sample.as_deref(),
        epoch: SHARED.epoch.load(Relaxed),
        fenced: init_small::instaux::fenced(),
        fence_reason: pgsync::lock(&SHARED.fence_reason).clone(),
        write_wedged: crate::liveness::write_wedged(now, limits.wedge_ms, external_wedged),
        limits,
    };
    f(&v)
}

fn probe(verdict: probes::Verdict) -> Response {
    Response::json(if verdict.ok { 200 } else { 503 }, status::render_probe(&verdict))
}

struct Route {
    method: &'static str,
    path: &'static str,
    job: Option<(JobKind, bool, u64)>,
}

const ROUTES: [Route; 11] = [
    Route { method: "GET", path: "/v1/status", job: None },
    Route { method: "GET", path: "/v1/role", job: None },
    Route { method: "GET", path: "/v1/probe/startup", job: None },
    Route { method: "GET", path: "/v1/probe/ready", job: None },
    Route { method: "GET", path: "/v1/probe/live", job: None },
    Route { method: "POST", path: "/v1/config/check", job: Some((JobKind::ConfigCheck, false, 30)) },
    Route { method: "POST", path: "/v1/config/apply", job: Some((JobKind::ConfigApply, true, 60)) },
    Route { method: "POST", path: "/v1/promote", job: Some((JobKind::Promote, true, 120)) },
    Route { method: "POST", path: "/v1/fence", job: Some((JobKind::Fence, true, 60)) },
    Route { method: "POST", path: "/v1/roles/apply", job: Some((JobKind::Roles, true, 120)) },
    Route { method: "POST", path: "/v1/tls/reload", job: Some((JobKind::TlsReload, true, 60)) },
];

pub(crate) fn handle(req: &Request, identities: &[String]) -> Response {
    if req.path == "/update" || req.path.ends_with("/update") {
        return Response::error(410, "refused", "in-place binary updates are refused; roll a new image instead");
    }
    let Some(route) = ROUTES.iter().find(|r| r.path == req.path) else {
        return Response::error(404, "not_found", &format!("no endpoint {}", req.path));
    };
    if route.method != req.method {
        return Response::error(405, "method_not_allowed", &format!("{} expects {}", route.path, route.method));
    }
    match route.job {
        None => with_view(|v| match route.path {
            "/v1/status" => Response::json(200, status::render(v)),
            "/v1/role" => Response::json(200, status::render_role(v)),
            "/v1/probe/startup" => probe(probes::startup(v)),
            "/v1/probe/ready" => probe(probes::ready(v)),
            _ => probe(probes::live(v)),
        }),
        Some((kind, admin, timeout_s)) => {
            if admin {
                let allowed = guc_tables::backing::pgrust_instance_aux_admin_identities().unwrap_or_default();
                if !crate::tls::admin_allowed(identities, &allowed) {
                    return Response::error(403, "forbidden", "the client certificate is not an admin identity");
                }
            }
            let caller = identities.first().cloned().unwrap_or_default();
            run_job(Job { kind, body: req.body.clone(), caller }, Duration::from_secs(timeout_s))
        }
    }
}

pub(crate) fn run_job(job: Job, timeout: Duration) -> Response {
    let mut slot = pgsync::lock(&SHARED.jobs);
    if slot.pending.is_some() || slot.running.is_some() {
        return Response::error(409, "another_request_in_progress", "another control request is running; retry");
    }
    let id = slot.next_id;
    slot.next_id += 1;
    slot.pending = Some((id, job));
    let deadline = now_ms() + timeout.as_millis() as i64;
    loop {
        if let Some((done_id, _)) = &slot.done {
            if *done_id == id {
                return slot.done.take().map(|(_, r)| r).unwrap_or_else(|| Response::error(500, "internal", "lost response"));
            }
        }
        let left = deadline - now_ms();
        if left <= 0 {
            if slot.pending.as_ref().is_some_and(|(p, _)| *p == id) {
                slot.pending = None;
                return Response::error(503, "aux_unavailable", "the instance aux process did not pick the request up");
            }
            return Response::error(504, "timeout", "the request is still running; poll /v1/status");
        }
        slot = SHARED
            .jobs_cv
            .wait_timeout(slot, Duration::from_millis(left.min(500) as u64))
            .map(|(g, _)| g)
            .unwrap_or_else(|e| e.into_inner().0);
    }
}

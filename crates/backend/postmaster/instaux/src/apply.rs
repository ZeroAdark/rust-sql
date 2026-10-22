use types_error::PgResult;

use crate::config::{self, Checked, ConfigState};
use crate::http::Response;
use crate::json::JsonOut;
use crate::shared::now_ms;

const RELOAD_WAIT_MS: i64 = 15_000;

pub(crate) struct Pending {
    pub checked: Checked,
    pub params: Vec<String>,
    pub after_gen: u64,
    pub deadline: i64,
}

/// Validates first, so an invalid value never reaches a file the next
/// restart would read; then writes atomically and asks for a reload.
pub(crate) fn begin(body: &[u8], reload_gen: u64) -> PgResult<Result<Pending, Response>> {
    let req = match config::parse_request(body) {
        Ok(r) => r,
        Err(resp) => return Ok(Err(resp)),
    };
    let checked = config::check(&req, transam_xlog::RecoveryInProgress());
    if !checked.refusals.is_empty() {
        return Ok(Err(config::check_response(&checked)));
    }
    if let Some(h) = &req.hash {
        if *h != checked.hash {
            return Ok(Err(Response::error(409, "hash_mismatch", &format!("rendered hash is {}, not {h}", checked.hash))));
        }
    }
    crate::files::write_durable(&config::managed_conf_path(), checked.rendered.as_bytes())?;
    if let Some(hba) = &req.hba {
        crate::files::write_durable(&hba::hba_file_name(), hba.as_bytes())?;
    }
    postmaster_seams::signal_postmaster_sighup::call();
    let params = req.params.into_iter().map(|(k, _)| k).collect();
    Ok(Ok(Pending { checked, params, after_gen: reload_gen, deadline: now_ms() + RELOAD_WAIT_MS }))
}

fn overridden(params: &[String]) -> Vec<(String, String)> {
    let Ok((_, vars)) = guc::process_config::process_config_file_internal_list(
        types_guc::GucContext::PGC_SIGHUP,
        false,
        types_error::DEBUG3,
    ) else {
        return Vec::new();
    };
    let managed = config::managed_conf_path();
    let mut out = Vec::new();
    for p in params {
        let last = vars.iter().rev().find(|v| v.name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(p)));
        let file = last.and_then(|v| v.filename.as_ref()).map(|f| f.display().to_string());
        if !file.as_deref().is_some_and(|f| f.ends_with(&managed)) {
            out.push((p.clone(), file.unwrap_or_else(|| "(not read)".into())));
        }
    }
    out
}

pub(crate) fn poll(p: &Pending, reload_gen: u64, state: &ConfigState) -> Option<Response> {
    if reload_gen <= p.after_gen {
        return (now_ms() > p.deadline)
            .then(|| Response::error(504, "reload_timeout", "the configuration was written but no reload was observed"));
    }
    let over = overridden(&p.params);
    let mut j = JsonOut::new();
    j.open(None);
    j.bool(Some("applied"), over.is_empty());
    j.opt_str(Some("applied_hash"), state.applied_hash.as_deref());
    config::checked_json(&p.checked, &mut j);
    j.open_arr(Some("pending_restart"));
    for n in &state.pending_restart {
        j.str(None, n);
    }
    j.close_arr();
    j.open_arr(Some("decreased_sensitive"));
    for (n, v) in &state.decreased {
        j.open(None);
        j.str(Some("parameter"), n);
        j.int(Some("value"), *v);
        j.close();
    }
    j.close_arr();
    j.bool(Some("restart_ready"), state.restart_ready);
    j.open_arr(Some("overridden"));
    for (n, f) in &over {
        j.open(None);
        j.str(Some("parameter"), n);
        j.str(Some("file"), f);
        j.close();
    }
    j.close_arr();
    j.close();
    Some(Response::json(if over.is_empty() { 200 } else { 409 }, j.finish()))
}

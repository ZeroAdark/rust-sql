use serde_json::Value;
use types_error::PgResult;

use crate::http::Response;
use crate::json::JsonOut;
use crate::shared::now_ms;

const PROMOTE_SIGNAL_FILE: &str = "promote";

pub(crate) fn begin(v: &Value) -> PgResult<Result<i64, Response>> {
    if !transam_xlog::RecoveryInProgress() {
        return Ok(Err(Response::error(409, "not_in_recovery", "the instance is already a primary")));
    }
    let expected = v.get("expected_term").and_then(Value::as_u64);
    if instaux_seams::promote_precheck::is_installed() {
        if let Some(Err(m)) = instaux_seams::promote_precheck::call_if_or(None, |f| Some(f(expected))) {
            return Ok(Err(Response::error(409, "term_mismatch", &m)));
        }
    } else if expected.is_some() {
        return Ok(Err(Response::error(409, "no_term_source", "expected_term given but this server has no term source")));
    }
    let wait = v.get("wait_seconds").and_then(Value::as_u64).unwrap_or(60).clamp(1, 600) as i64;
    let file = fd::AllocateFile(PROMOTE_SIGNAL_FILE, "w")?;
    fd::FreeFile(file)?;
    postmaster_seams::signal_postmaster_sigusr1::call();
    Ok(Ok(now_ms() + wait * 1000))
}

pub(crate) fn poll(deadline: i64) -> Option<Response> {
    if !transam_xlog::RecoveryInProgress() {
        let term = instaux_seams::external_sample::call_if_or(None, |f| f().term);
        let mut j = JsonOut::new();
        j.open(None);
        j.bool(Some("promoted"), true);
        j.uint(Some("timeline"), transam_xlog::ctl::GetWALInsertionTimeLine() as u64);
        j.opt_uint(Some("term"), term);
        j.close();
        return Some(Response::json(200, j.finish()));
    }
    (now_ms() > deadline).then(|| Response::error(504, "promotion_timeout", "the server did not finish promoting in time"))
}

use serde_json::Value;
use types_core::init::BackendType;
use types_error::{PgResult, LOG};

use crate::http::Response;
use crate::json::JsonOut;

fn session_mask() -> u32 {
    (1u32 << BackendType::Backend as u32) | (1u32 << BackendType::WalSender as u32)
}

/// Fencing refuses new client connections and terminates every session;
/// the server itself, recovery and the aux keep running.
pub(crate) fn run(v: &Value, caller: &str) -> PgResult<Response> {
    let Some(fenced) = v.get("fenced").and_then(Value::as_bool) else {
        return Ok(Response::error(400, "bad_request", "\"fenced\" (boolean) is required"));
    };
    let reason = v.get("reason").and_then(Value::as_str).unwrap_or("");
    let mut p = crate::state::current();
    p.fenced = fenced;
    p.reason = if fenced { reason.to_string() } else { String::new() };
    crate::state::store(&p)?;
    let terminated = if fenced {
        let n = pmchild_seams::count_children::call(session_mask());
        pmchild_seams::signal_children::call(procsignal::signums::SIGTERM, session_mask());
        n
    } else {
        0
    };
    instaux_seams::fence_changed::call_if(|f| f(fenced));
    let _ = elog::elog(
        LOG,
        format!(
            "instance aux: {} by \"{caller}\"{}",
            if fenced { "fenced" } else { "unfenced" },
            if reason.is_empty() { String::new() } else { format!(": {reason}") }
        ),
    );
    let mut j = JsonOut::new();
    j.open(None);
    j.bool(Some("fenced"), fenced);
    j.int(Some("terminated"), terminated as i64);
    j.close();
    Ok(Response::json(200, j.finish()))
}

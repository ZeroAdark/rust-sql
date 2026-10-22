use std::sync::atomic::Ordering::Relaxed;

use serde_json::Value;

use crate::http::Response;
use crate::shared::SHARED;

pub(crate) fn body_json(body: &[u8]) -> Result<Value, Response> {
    serde_json::from_slice(body).map_err(|e| Response::error(400, "bad_request", &format!("invalid JSON: {e}")))
}

/// Every mutation names the controller epoch and this instance's
/// incarnation: an older epoch is a deposed controller, another incarnation
/// a command meant for a replaced instance.
pub(crate) fn admit(v: &Value) -> Result<(), Response> {
    let Some(epoch) = v.get("epoch").and_then(Value::as_u64) else {
        return Err(Response::error(400, "bad_request", "\"epoch\" (unsigned integer) is required"));
    };
    let Some(incarnation) = v.get("incarnation").and_then(Value::as_str) else {
        return Err(Response::error(400, "bad_request", "\"incarnation\" (string) is required"));
    };
    let mine = guc_tables::backing::pgrust_instance_aux_incarnation().unwrap_or_default();
    if incarnation != mine {
        return Err(Response::error(
            409,
            "wrong_incarnation",
            &format!("this is incarnation \"{mine}\", not \"{incarnation}\""),
        ));
    }
    let current = SHARED.epoch.load(Relaxed);
    if epoch < current {
        return Err(Response::error(409, "stale_epoch", &format!("epoch {epoch} is older than {current}")));
    }
    if epoch > current {
        let mut p = crate::state::current();
        p.epoch = epoch;
        crate::state::store(&p)
            .map_err(|e| Response::error(500, "state_write_failed", e.message()))?;
    }
    Ok(())
}

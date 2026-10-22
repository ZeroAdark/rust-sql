// Portions derived from CloudNativePG v1.30.1 (pkg/configfile/configfile.go,
// pkg/management/postgres/probes.go), Copyright © contributors to CloudNativePG,
// established as CloudNativePG a Series of LF Projects, LLC. Licensed under the
// Apache License, Version 2.0. Modified by PagerFree, Inc., 2026: translated to Rust.
// SPDX-License-Identifier: Apache-2.0

use serde_json::Value;
use types_error::{PgResult, DEBUG5};
use types_guc::{GucContext, GucSource, GUC_DISALLOW_IN_FILE};

use crate::http::Response;
use crate::json::JsonOut;

pub(crate) const HOT_STANDBY_SENSITIVE: [&str; 5] = [
    "max_connections",
    "max_prepared_transactions",
    "max_wal_senders",
    "max_worker_processes",
    "max_locks_per_transaction",
];

const FORBIDDEN: [&str; 7] =
    ["config_file", "data_directory", "hba_file", "ident_file", "include", "include_dir", "include_if_exists"];

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Refusal {
    pub parameter: Option<String>,
    pub code: &'static str,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Reorder {
    pub parameter: String,
    pub from: i64,
    pub to: i64,
    pub order: &'static str,
}

#[derive(Debug, Default)]
pub(crate) struct Checked {
    pub refusals: Vec<Refusal>,
    pub requires_restart: Vec<String>,
    pub reorders: Vec<Reorder>,
    pub standby_blocked: Vec<String>,
    pub rendered: String,
    pub hash: String,
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Request {
    pub params: Vec<(String, String)>,
    pub hba: Option<String>,
    pub hash: Option<String>,
}

pub(crate) fn parse_request(body: &[u8]) -> Result<Request, Response> {
    let bad = |m: &str| Response::error(400, "bad_request", m);
    let v: Value = serde_json::from_slice(body).map_err(|e| bad(&format!("invalid JSON: {e}")))?;
    let mut req = Request::default();
    match v.get("parameters") {
        None | Some(Value::Null) => {}
        Some(Value::Object(m)) => {
            for (k, val) in m {
                let text = match val {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => if *b { "on" } else { "off" }.to_string(),
                    _ => return Err(bad(&format!("parameter \"{k}\" must be a string, number or boolean"))),
                };
                req.params.push((k.clone(), text));
            }
        }
        Some(_) => return Err(bad("\"parameters\" must be an object")),
    }
    req.params.sort();
    req.hba = match v.get("hba") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(bad("\"hba\" must be a string")),
    };
    req.hash = v.get("hash").and_then(Value::as_str).map(str::to_string);
    Ok(req)
}

pub(crate) fn name_shape_ok(name: &str) -> bool {
    !name.is_empty()
        && name.split('.').all(|part| {
            let mut bytes = part.bytes();
            matches!(bytes.next(), Some(b) if b.is_ascii_alphabetic() || b == b'_')
                && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'$')
        })
}

fn escape_value(v: &str) -> String {
    let mut out = String::from("'");
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("''"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

pub(crate) fn render(params: &[(String, String)]) -> String {
    let mut out = String::from("# Written by the instance aux on configuration apply; do not edit.\n");
    for (k, v) in params {
        out.push_str(k);
        out.push_str(" = ");
        out.push_str(&escape_value(v));
        out.push('\n');
    }
    out
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    openssl::sha::sha256(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

enum Parsed {
    Int(i64),
    Other,
}

fn refuse(p: &str, code: &'static str, message: String) -> Refusal {
    Refusal { parameter: Some(p.to_string()), code, message }
}

fn check_one(name: &str, value: &str) -> Result<(Parsed, bool), Refusal> {
    if !name_shape_ok(name) {
        return Err(refuse(name, "invalid_name", format!("invalid configuration parameter name \"{name}\"")));
    }
    let lower = name.to_ascii_lowercase();
    if FORBIDDEN.contains(&lower.as_str()) || lower.starts_with("pgrust.instance_aux") {
        return Err(refuse(name, "forbidden_parameter", format!("parameter \"{name}\" is managed by the instance launcher")));
    }
    if value.contains('\0') {
        return Err(refuse(name, "invalid_value", format!("value of parameter \"{name}\" contains a NUL byte")));
    }
    let found = guc::with_store(|reg| {
        reg.find_option(name).map(|record| {
            let gen = record.gen();
            if gen.context == GucContext::PGC_INTERNAL || gen.flags & GUC_DISALLOW_IN_FILE != 0 {
                return Err(refuse(name, "not_settable", format!("parameter \"{name}\" cannot be set in a configuration file")));
            }
            let restart = gen.context == GucContext::PGC_POSTMASTER;
            match guc::parse_and_validate_value(record, value, GucSource::PGC_S_FILE) {
                Ok((guc::model::config_var_val::Intval(i), _)) => Ok((Parsed::Int(i as i64), restart)),
                Ok((guc::model::config_var_val::Enumval(i), _)) => Ok((Parsed::Int(i as i64), restart)),
                Ok((guc::model::config_var_val::Boolval(b), _)) => Ok((Parsed::Int(b as i64), restart)),
                Ok(_) => Ok((Parsed::Other, restart)),
                Err(e) => Err(refuse(name, "invalid_value", e.message().to_string())),
            }
        })
    })
    .flatten();
    if let Some(r) = found {
        return r;
    }
    if !name.contains('.') {
        return Err(refuse(name, "unrecognized_parameter", format!("unrecognized configuration parameter \"{name}\"")));
    }
    if lower.starts_with("pgrust.") {
        return Err(refuse(name, "unrecognized_parameter", format!("unrecognized configuration parameter \"{name}\"")));
    }
    match guc::assignable_custom_variable_name(name, false) {
        Ok(_) => Ok((Parsed::Other, false)),
        Err(e) => Err(refuse(name, "reserved_prefix", e.message().to_string())),
    }
}

fn current_int(name: &str) -> Option<i64> {
    guc::get_int(name).map(i64::from).or_else(|| guc::get_enum(name).map(i64::from)).or_else(|| guc::get_bool(name).map(i64::from))
}

pub(crate) struct Effective<'a> {
    parsed: &'a [(String, i64)],
}

impl<'a> Effective<'a> {
    #[cfg(test)]
    pub(crate) fn for_tests(parsed: &'a [(String, i64)]) -> Self {
        Effective { parsed }
    }

    fn get(&self, name: &str) -> i64 {
        self.parsed.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| *v).or_else(|| current_int(name)).unwrap_or(0)
    }
}

pub(crate) fn startup_checks(e: &Effective<'_>, wal_seg_mb: i64) -> Vec<Refusal> {
    let mut out = Vec::new();
    let (su, rsv, maxc) = (e.get("superuser_reserved_connections"), e.get("reserved_connections"), e.get("max_connections"));
    if su + rsv >= maxc {
        out.push(Refusal { parameter: None, code: "startup_check", message: format!("\"superuser_reserved_connections\" ({su}) plus \"reserved_connections\" ({rsv}) must be less than \"max_connections\" ({maxc})") });
    }
    let minimal = e.get("wal_level") == 0;
    if minimal && e.get("archive_mode") > 0 {
        out.push(Refusal { parameter: None, code: "startup_check", message: "WAL archival cannot be enabled when \"wal_level\" is \"minimal\"".into() });
    }
    if minimal && e.get("max_wal_senders") > 0 {
        out.push(Refusal { parameter: None, code: "startup_check", message: "WAL streaming (\"max_wal_senders\" > 0) requires \"wal_level\" to be \"replica\" or \"logical\"".into() });
    }
    if minimal && e.get("summarize_wal") != 0 {
        out.push(Refusal { parameter: None, code: "startup_check", message: "WAL cannot be summarized when \"wal_level\" is \"minimal\"".into() });
    }
    if wal_seg_mb > 0 {
        for p in ["min_wal_size", "max_wal_size"] {
            if e.get(p) < 2 * wal_seg_mb {
                out.push(Refusal { parameter: Some(p.into()), code: "startup_check", message: format!("\"{p}\" must be at least twice \"wal_segment_size\"") });
            }
        }
    }
    out
}

pub(crate) fn reorders(parsed: &[(String, i64)]) -> Vec<Reorder> {
    let mut out = Vec::new();
    for name in HOT_STANDBY_SENSITIVE {
        let Some((_, to)) = parsed.iter().find(|(n, _)| n == name) else { continue };
        let Some(from) = current_int(name) else { continue };
        let order = match to.cmp(&from) {
            std::cmp::Ordering::Less => "primary_first",
            std::cmp::Ordering::Greater => "standbys_first",
            std::cmp::Ordering::Equal => continue,
        };
        out.push(Reorder { parameter: name.into(), from, to: *to, order });
    }
    out
}

pub(crate) fn primary_values() -> [i64; 5] {
    let cf = transam_xlog::control_file::control_file();
    [
        cf.MaxConnections as i64,
        cf.max_prepared_xacts as i64,
        cf.max_wal_senders as i64,
        cf.max_worker_processes as i64,
        cf.max_locks_per_xact as i64,
    ]
}

pub(crate) fn check(req: &Request, in_recovery: bool) -> Checked {
    let mut c = Checked::default();
    let mut parsed: Vec<(String, i64)> = Vec::new();
    for (name, value) in &req.params {
        match check_one(name, value) {
            Ok((p, restart)) => {
                if let Parsed::Int(i) = p {
                    parsed.push((name.to_ascii_lowercase(), i));
                }
                if restart {
                    c.requires_restart.push(name.clone());
                }
            }
            Err(r) => c.refusals.push(r),
        }
    }
    let wal_seg_mb = transam_xlog::control_file::control_file().xlog_seg_size as i64 / (1024 * 1024);
    if c.refusals.is_empty() {
        c.refusals = startup_checks(&Effective { parsed: &parsed }, wal_seg_mb);
    }
    if let Some(hba) = &req.hba {
        c.refusals.extend(check_hba(hba));
    }
    c.reorders = reorders(&parsed);
    if in_recovery {
        let primary = primary_values();
        for (i, name) in HOT_STANDBY_SENSITIVE.iter().enumerate() {
            if let Some((_, v)) = parsed.iter().find(|(n, _)| n == name) {
                if *v < primary[i] {
                    c.standby_blocked.push((*name).to_string());
                }
            }
        }
    }
    c.rendered = render(&req.params);
    c.hash = sha256_hex(c.rendered.as_bytes());
    c
}

pub(crate) fn check_hba(text: &str) -> Vec<Refusal> {
    let path = format!("{}.instaux-check", hba::hba_file_name());
    let bad = |message: String| Refusal { parameter: None, code: "hba_invalid", message };
    let mut out = Vec::new();
    if let Err(e) = crate::files::write_durable(&path, text.as_bytes()) {
        out.push(bad(e.message().to_string()));
        return out;
    }
    let mut open_err = None;
    let result: PgResult<usize> = (|| {
        let Some(file) = hba::open_auth_file(&path, DEBUG5, 0, &mut open_err)? else {
            return Ok(0);
        };
        let mut lines = Vec::new();
        hba::tokenize_auth_file(&path, &file, &mut lines, DEBUG5, 0)?;
        let mut parsed = 0;
        for line in lines.iter_mut() {
            if line.err_msg.is_none() && hba::parse_hba_line(line, DEBUG5)?.is_some() {
                parsed += 1;
                continue;
            }
            let why = line.err_msg.clone().unwrap_or_else(|| "invalid entry".into());
            out.push(bad(format!("line {}: {why}", line.line_num)));
        }
        Ok(parsed)
    })();
    if let Ok(c) = std::ffi::CString::new(path.as_str()) {
        vfs::unlink(&c);
    }
    match result {
        Ok(0) if out.is_empty() => out.push(bad(open_err.unwrap_or_else(|| "pg_hba.conf contains no entries".into()))),
        Err(e) => out.push(bad(e.message().to_string())),
        _ => {}
    }
    out
}

pub(crate) fn checked_json(c: &Checked, j: &mut JsonOut) {
    j.bool(Some("valid"), c.refusals.is_empty());
    j.str(Some("hash"), &c.hash);
    j.open_arr(Some("refusals"));
    for r in &c.refusals {
        j.open(None);
        j.opt_str(Some("parameter"), r.parameter.as_deref());
        j.str(Some("code"), r.code);
        j.str(Some("message"), &r.message);
        j.close();
    }
    j.close_arr();
    j.open_arr(Some("requires_restart"));
    for p in &c.requires_restart {
        j.str(None, p);
    }
    j.close_arr();
    j.open_arr(Some("restart_order"));
    for r in &c.reorders {
        j.open(None);
        j.str(Some("parameter"), &r.parameter);
        j.int(Some("from"), r.from);
        j.int(Some("to"), r.to);
        j.str(Some("order"), r.order);
        j.close();
    }
    j.close_arr();
    j.open_arr(Some("standby_restart_blocked"));
    for p in &c.standby_blocked {
        j.str(None, p);
    }
    j.close_arr();
}

pub(crate) fn check_response(c: &Checked) -> Response {
    let mut j = JsonOut::new();
    j.open(None);
    checked_json(c, &mut j);
    j.close();
    Response::json(if c.refusals.is_empty() { 200 } else { 422 }, j.finish())
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ConfigState {
    pub applied_hash: Option<String>,
    pub pending_restart: Vec<String>,
    pub decreased: Vec<(String, i64)>,
    pub restart_ready: bool,
}

pub(crate) fn managed_conf_path() -> String {
    guc_tables::backing::pgrust_instance_aux_managed_conf().unwrap_or_default()
}

pub(crate) fn current_state(in_recovery: bool) -> ConfigState {
    let applied_hash = crate::files::read_all(&managed_conf_path()).map(|b| sha256_hex(&b));
    let mut pending_restart: Vec<String> = Vec::new();
    guc::with_store(|reg| {
        for v in reg.iter() {
            if v.gen().status & guc::model::GUC_PENDING_RESTART != 0 {
                pending_restart.push(v.name().to_string());
            }
        }
    });
    pending_restart.sort();
    let decreased = if pending_restart.iter().any(|p| HOT_STANDBY_SENSITIVE.contains(&p.as_str())) {
        decreased_sensitive(&pending_restart)
    } else {
        Vec::new()
    };
    let restart_ready = !in_recovery || {
        let primary = primary_values();
        decreased.iter().all(|(n, v)| {
            HOT_STANDBY_SENSITIVE.iter().position(|s| s == n).is_some_and(|i| primary[i] == *v)
        })
    };
    ConfigState { applied_hash, pending_restart, decreased, restart_ready }
}

fn decreased_sensitive(pending: &[String]) -> Vec<(String, i64)> {
    let Ok((_, vars)) = guc::process_config::process_config_file_internal_list(
        GucContext::PGC_SIGHUP,
        false,
        types_error::DEBUG3,
    ) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for name in HOT_STANDBY_SENSITIVE {
        if !pending.iter().any(|p| p == name) {
            continue;
        }
        let file_value = vars.iter().rev().find(|v| v.name.as_deref() == Some(name)).and_then(|v| v.value.clone());
        let new = file_value.and_then(|text| {
            guc::with_store(|reg| {
                reg.find_option(name).and_then(|r| match guc::parse_and_validate_value(r, &text, GucSource::PGC_S_FILE) {
                    Ok((guc::model::config_var_val::Intval(i), _)) => Some(i as i64),
                    _ => None,
                })
            })
            .flatten()
        });
        if let (Some(new), Some(cur)) = (new, current_int(name)) {
            if new < cur {
                out.push((name.to_string(), new));
            }
        }
    }
    out
}

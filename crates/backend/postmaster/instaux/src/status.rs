// Portions derived from CloudNativePG v1.30.1 (pkg/postgres/status.go), Copyright ©
// contributors to CloudNativePG, established as CloudNativePG a Series of LF Projects,
// LLC. Licensed under the Apache License, Version 2.0. Modified by PagerFree, Inc.,
// 2026: translated to Rust and reshaped for term-fenced instances.
// SPDX-License-Identifier: Apache-2.0

use instaux_seams::ExtValue;

use crate::json::JsonOut;
use crate::names::pm_state_name;
use crate::probes::{self, View};
use crate::sample::Usage;
use crate::tls::CertInfo;

pub(crate) use crate::names::STATUS_SCHEMA;

fn usage(j: &mut JsonOut, key: &str, u: Option<Usage>) {
    match u {
        None => j.null(Some(key)),
        Some(u) => {
            j.open(Some(key));
            j.uint(Some("total_bytes"), u.total_bytes);
            j.uint(Some("avail_bytes"), u.avail_bytes);
            j.close();
        }
    }
}

fn cert(j: &mut JsonOut, key: &str, c: Option<&CertInfo>) {
    match c {
        None => j.null(Some(key)),
        Some(c) => {
            j.open(Some(key));
            j.str(Some("serial"), &c.serial);
            j.int(Some("not_after"), c.not_after);
            j.close();
        }
    }
}

fn verdict(j: &mut JsonOut, key: &str, v: &probes::Verdict) {
    j.open(Some(key));
    j.bool(Some("ok"), v.ok);
    j.opt_str(Some("reason"), v.reason.as_deref());
    j.close();
}

pub(crate) fn render(v: &View<'_>) -> String {
    let s = v.sample;
    let mut j = JsonOut::new();
    j.open(None);
    j.str(Some("schema"), STATUS_SCHEMA);
    j.open(Some("identity"));
    j.opt_str(Some("cluster"), s.map(|s| s.identity.cluster.as_str()));
    j.opt_str(Some("instance"), s.map(|s| s.identity.instance.as_str()));
    j.opt_str(Some("incarnation"), s.map(|s| s.identity.incarnation.as_str()));
    j.opt_str(Some("system_identifier"), s.map(|s| s.identity.system_identifier.to_string()).as_deref());
    j.opt_str(Some("version"), s.map(|s| s.identity.version.as_str()));
    j.close();
    j.str(Some("role"), probes::role(v));
    j.str(Some("server_state"), pm_state_name(v.pm_state));
    j.bool(Some("crash_restart"), v.fatal_error);
    j.bool(Some("fenced"), v.fenced);
    j.opt_str(Some("fence_reason"), (v.fenced && !v.fence_reason.is_empty()).then_some(v.fence_reason.as_str()));
    j.uint(Some("epoch"), v.epoch);
    j.opt_uint(Some("term"), s.and_then(|s| s.external.term));
    j.opt_uint(Some("timeline"), s.map(|s| s.timeline as u64));
    j.open(Some("lsn"));
    j.opt_lsn(Some("insert"), s.and_then(|s| s.insert_lsn));
    j.opt_lsn(Some("flush"), s.and_then(|s| s.flush_lsn));
    j.opt_lsn(Some("receive"), s.and_then(|s| s.receive_lsn));
    j.opt_lsn(Some("replay"), s.and_then(|s| s.replay_lsn));
    j.opt_lsn(Some("durable"), s.and_then(|s| s.external.durable_lsn));
    j.opt_lsn(Some("log_end"), s.and_then(|s| s.external.log_end_lsn));
    j.close();
    j.open(Some("lag"));
    j.opt_uint(Some("bytes"), s.and_then(|s| s.lag_bytes));
    j.opt_int(Some("ms"), s.and_then(|s| s.lag_ms));
    j.close();
    j.open(Some("liveness"));
    j.opt_str(Some("source"), s.map(|s| s.live.source));
    j.opt_uint(Some("seq"), s.map(|s| s.live.seq));
    j.opt_lsn(Some("lsn"), s.and_then(|s| s.live.lsn));
    j.opt_int(Some("commit_age_ms"), probes::commit_age(v));
    j.bool(Some("write_wedged"), v.write_wedged);
    j.close();
    j.open(Some("probes"));
    verdict(&mut j, "startup", &probes::startup(v));
    verdict(&mut j, "ready", &probes::ready(v));
    verdict(&mut j, "live", &probes::live(v));
    j.close();
    j.open(Some("config"));
    j.opt_str(Some("applied_hash"), s.and_then(|s| s.config.applied_hash.as_deref()));
    j.open_arr(Some("pending_restart"));
    for p in s.map(|s| s.config.pending_restart.as_slice()).unwrap_or(&[]) {
        j.str(None, p);
    }
    j.close_arr();
    j.open_arr(Some("decreased_sensitive"));
    for (p, val) in s.map(|s| s.config.decreased.as_slice()).unwrap_or(&[]) {
        j.open(None);
        j.str(Some("parameter"), p);
        j.int(Some("value"), *val);
        j.close();
    }
    j.close_arr();
    j.bool(Some("restart_ready"), s.is_none_or(|s| s.config.restart_ready));
    j.close();
    j.open(Some("disk"));
    usage(&mut j, "data", s.and_then(|s| s.data_disk));
    usage(&mut j, "wal", s.and_then(|s| s.wal_disk));
    j.opt_uint(Some("wal_budget_bytes"), s.map(|s| s.wal_budget_bytes));
    j.close();
    j.open(Some("connections"));
    j.opt_int(Some("current"), s.map(|s| s.connections as i64));
    j.opt_int(Some("max"), s.map(|s| s.max_connections as i64));
    j.close();
    j.open(Some("tls"));
    cert(&mut j, "aux", s.and_then(|s| s.tls_aux.as_ref()));
    cert(&mut j, "server", s.and_then(|s| s.tls_server.as_ref()));
    j.close();
    j.opt_int(Some("sample_age_ms"), s.map(|s| v.now_ms - s.taken_ms));
    j.open(Some("ext"));
    for (k, val) in s.map(|s| s.external.ext.as_slice()).unwrap_or(&[]) {
        match val {
            ExtValue::Bool(b) => j.bool(Some(k), *b),
            ExtValue::Int(i) => j.int(Some(k), *i),
            ExtValue::Text(t) => j.str(Some(k), t),
        }
    }
    j.close();
    j.close();
    j.finish()
}

pub(crate) fn render_role(v: &View<'_>) -> String {
    let mut j = JsonOut::new();
    j.open(None);
    j.str(Some("role"), probes::role(v));
    j.opt_uint(Some("term"), v.sample.and_then(|s| s.external.term));
    j.opt_uint(Some("timeline"), v.sample.map(|s| s.timeline as u64));
    j.bool(Some("fenced"), v.fenced);
    j.opt_str(Some("incarnation"), v.sample.map(|s| s.identity.incarnation.as_str()));
    j.close();
    j.finish()
}

pub(crate) fn render_probe(v: &probes::Verdict) -> String {
    let mut j = JsonOut::new();
    j.open(None);
    j.bool(Some("ok"), v.ok);
    j.opt_str(Some("reason"), v.reason.as_deref());
    j.close();
    j.finish()
}

use std::ffi::CString;

use instaux_seams::ExternalSample;
use types_core::InvalidOid;

use crate::config::ConfigState;
use crate::liveness::{LiveView, Liveness};
use crate::shared::now_ms;
use crate::tls::CertInfo;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Recovery {
    Crash,
    Archive,
    Done,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Identity {
    pub cluster: String,
    pub instance: String,
    pub incarnation: String,
    pub system_identifier: u64,
    pub version: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Usage {
    pub total_bytes: u64,
    pub avail_bytes: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Sample {
    pub taken_ms: i64,
    pub identity: Identity,
    pub recovery: Recovery,
    pub hot_standby: bool,
    pub timeline: u32,
    pub insert_lsn: Option<u64>,
    pub flush_lsn: Option<u64>,
    pub receive_lsn: Option<u64>,
    pub replay_lsn: Option<u64>,
    pub progress_ms: i64,
    pub external: ExternalSample,
    pub live: LiveView,
    pub lag_bytes: Option<u64>,
    pub lag_ms: Option<i64>,
    pub config: ConfigState,
    pub data_disk: Option<Usage>,
    pub wal_disk: Option<Usage>,
    pub wal_budget_bytes: u64,
    pub connections: i32,
    pub max_connections: i32,
    pub tls_aux: Option<CertInfo>,
    pub tls_server: Option<CertInfo>,
}

#[derive(Default)]
pub(crate) struct Sampler {
    last_replay: u64,
    progress_ms: i64,
    caught_up_ms: Option<i64>,
    identity: Identity,
}

pub(crate) fn statvfs(path: &str) -> Option<Usage> {
    let c = CString::new(path).ok()?;
    // SAFETY: `st` is plain data statvfs fills in; `c` is a valid C string.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let frsize = st.f_frsize as u64;
    Some(Usage { total_bytes: st.f_blocks as u64 * frsize, avail_bytes: st.f_bavail as u64 * frsize })
}

fn text_guc(v: Option<String>) -> String {
    v.unwrap_or_default()
}

impl Sampler {
    pub(crate) fn new() -> Self {
        use guc_tables::backing as b;
        let identity = Identity {
            cluster: text_guc(b::pgrust_instance_aux_cluster()),
            instance: text_guc(b::pgrust_instance_aux_instance()),
            incarnation: text_guc(b::pgrust_instance_aux_incarnation()),
            system_identifier: transam_xlog::control_file::GetSystemIdentifier(),
            version: format!("{} (PostgreSQL {})", guc_tables::consts::PGRUST_VERSION, guc_tables::consts::PG_COMPAT_VERSION),
        };
        Sampler { progress_ms: now_ms(), identity, ..Default::default() }
    }

    pub(crate) fn take(&mut self, live: &Liveness, config: &ConfigState) -> Sample {
        let now = now_ms();
        let recovery = match transam_xlog::GetRecoveryState() {
            transam_xlog::RECOVERY_STATE_CRASH => Recovery::Crash,
            transam_xlog::RECOVERY_STATE_ARCHIVE => Recovery::Archive,
            _ => Recovery::Done,
        };
        let external = instaux_seams::external_sample::call_if_or(ExternalSample::default(), |f| f());
        let (mut insert_lsn, mut flush_lsn, mut receive_lsn, mut replay_lsn) = (None, None, None, None);
        let timeline;
        if recovery == Recovery::Done {
            insert_lsn = Some(transam_xlog::GetXLogInsertRecPtr());
            flush_lsn = Some(transam_xlog::GetFlushRecPtr(None));
            timeline = transam_xlog::ctl::GetWALInsertionTimeLine();
        } else {
            let (replay, tli) = xlogrecovery::GetXLogReplayRecPtr();
            replay_lsn = Some(replay);
            timeline = tli;
            let (received, _, _) = walreceiverfuncs::GetWalRcvFlushRecPtr();
            receive_lsn = (received != 0).then_some(received);
            if replay != self.last_replay {
                self.last_replay = replay;
                self.progress_ms = now;
            }
        }
        let (lag_bytes, lag_ms) = match (recovery, replay_lsn) {
            (Recovery::Archive, Some(replay)) => {
                let end = external.log_end_lsn.or(receive_lsn).unwrap_or(replay);
                if replay >= end {
                    self.caught_up_ms = Some(now);
                }
                let since = self.caught_up_ms.unwrap_or(self.progress_ms);
                (Some(end.saturating_sub(replay)), Some(if replay >= end { 0 } else { now - since }))
            }
            _ => (None, None),
        };
        let ssl_cert = guc_tables::vars::ssl_cert_file.read().unwrap_or_default();
        Sample {
            taken_ms: now,
            identity: self.identity.clone(),
            recovery,
            hot_standby: xlogrecovery::HotStandbyActive(),
            timeline,
            insert_lsn,
            flush_lsn,
            receive_lsn,
            replay_lsn,
            progress_ms: self.progress_ms,
            live: live.view(external.durable_lsn),
            external,
            lag_bytes,
            lag_ms,
            config: config.clone(),
            data_disk: statvfs("."),
            wal_disk: statvfs("pg_wal"),
            wal_budget_bytes: guc_tables::vars::max_wal_size_mb.read().max(0) as u64 * 1024 * 1024,
            connections: procarray::CountDBConnections(InvalidOid).unwrap_or(-1),
            max_connections: init_small::globals::MaxConnections(),
            tls_aux: crate::shared::acceptor().and_then(|a| crate::tls::acceptor_info(&a)),
            tls_server: if guc_tables::vars::EnableSSL.read() { crate::tls::file_info(&ssl_cert) } else { None },
        }
    }
}

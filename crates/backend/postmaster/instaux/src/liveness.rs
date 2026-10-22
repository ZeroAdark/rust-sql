use std::collections::VecDeque;
use std::sync::atomic::Ordering::Relaxed;

use types_error::PgResult;

use crate::shared::{now_ms, SHARED};

const RING: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Commit {
    seq: u64,
    lsn: u64,
    at_ms: i64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct LiveView {
    pub source: &'static str,
    pub seq: u64,
    pub lsn: Option<u64>,
    pub acked_ms: Option<i64>,
    pub external_wedged: bool,
}

pub(crate) struct Liveness {
    next_seq: u64,
    ring: VecDeque<Commit>,
    last_ms: i64,
}

impl Liveness {
    pub(crate) fn new() -> Self {
        SHARED.commit_started_ms.store(0, Relaxed);
        let next_seq = (pg_clock::wall_ns() / 1_000_000).max(1) as u64;
        Liveness { next_seq, ring: VecDeque::with_capacity(RING), last_ms: 0 }
    }

    pub(crate) fn due(&self, now: i64) -> bool {
        now - self.last_ms >= guc_tables::backing::pgrust_instance_aux_liveness_interval() as i64
    }

    /// A WAL no-op flushed locally: write progress, not process liveness.
    pub(crate) fn commit(&mut self) -> PgResult<()> {
        let started = now_ms();
        self.last_ms = started;
        SHARED.commit_started_ms.store(started, Relaxed);
        let seq = self.next_seq;
        let lsn = xloginsert::insert_record(
            transam_xlog::RM_XLOG_ID,
            transam_xlog::XLOG_NOOP,
            transam_xlog::XLOG_MARK_UNIMPORTANT,
            &[&seq.to_le_bytes()],
            &[],
        )?;
        transam_xlog::XLogFlush(lsn)?;
        SHARED.commit_done_ms.store(now_ms(), Relaxed);
        SHARED.commit_started_ms.store(0, Relaxed);
        self.next_seq += 1;
        if self.ring.len() == RING {
            self.ring.pop_front();
        }
        self.ring.push_back(Commit { seq, lsn, at_ms: started });
        instaux_seams::liveness_committed::call_if(|f| f(seq, lsn));
        Ok(())
    }

    pub(crate) fn view(&self, durable_lsn: Option<u64>) -> LiveView {
        if let Some(Some(ext)) =
            instaux_seams::liveness::call_if_or(None, |f| Some(f()))
        {
            return LiveView {
                source: "external",
                seq: ext.seq,
                lsn: Some(ext.lsn),
                acked_ms: ext.acked_ms,
                external_wedged: ext.wedged,
            };
        }
        let newest = self.ring.back();
        let acked = match durable_lsn {
            Some(d) => self.ring.iter().rev().find(|c| c.lsn <= d),
            None => newest,
        };
        LiveView {
            source: "local",
            seq: newest.map_or(0, |c| c.seq),
            lsn: newest.map(|c| c.lsn),
            acked_ms: acked.map(|c| c.at_ms),
            external_wedged: false,
        }
    }
}

pub(crate) fn write_wedged(now: i64, wedge_ms: i64, external_wedged: bool) -> bool {
    let started = SHARED.commit_started_ms.load(Relaxed);
    external_wedged || (started != 0 && now - started > wedge_ms)
}

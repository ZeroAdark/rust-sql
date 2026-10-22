//! Each compaction job's costs, per table in process memory (a restart starts again). The CPU is the job's own thread's
//! (RUSAGE_THREAD): the seal pool's encode lanes are not in it.

use std::collections::BTreeMap;

use pgrc_compact::policy::{JobKind, Reason};
use types_core::Oid;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cost {
    pub inputs: u64,
    pub live_in: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub rows: u64,
    pub dropped: u64,
    pub thread_cpu_us: u64,
    pub wal_bytes: u64,
    pub publish_us: u64,
}

pub const FIELDS: [&str; 9] = ["inputs", "live_in", "bytes_in", "bytes_out", "rows", "dropped", "thread_cpu_us", "wal_bytes", "publish_us"];

impl Cost {
    fn values(&self) -> [u64; 9] {
        [self.inputs, self.live_in, self.bytes_in, self.bytes_out, self.rows, self.dropped, self.thread_cpu_us, self.wal_bytes, self.publish_us]
    }
}

pub const LAST: usize = 8;
pub const TABLES: usize = 1024;

#[derive(Clone, Copy)]
struct Done {
    kind: JobKind,
    reason: Reason,
    cost: [u64; 9],
}

struct Entry {
    seq: u64,
    sums: [(u64, [u64; 9]); 6],
    last: Vec<Done>,
    sh: [[u64; 5]; 2],
    cold: u64,
    first: Option<[f64; 2]>,
}

impl Entry {
    fn new(seq: u64) -> Entry {
        Entry { seq, sums: [(0, [0; 9]); 6], last: Vec::with_capacity(LAST), sh: [[0; 5]; 2], cold: 0, first: None }
    }
}

pub const SH_FIELDS: [&str; 5] = ["scored", "would_skip", "would_add", "would_reorder", "add_now"];
const SCORED: usize = 0;
const SKIP: usize = 1;
const ADD: usize = 2;
const REORDER: usize = 3;
const ADD_NOW: usize = 4;

pub struct Ledger {
    seq: u64,
    map: BTreeMap<(Oid, Oid), Entry>,
    sweeps: u64,
    sweep_us: u64,
}

impl Default for Ledger {
    fn default() -> Ledger {
        Ledger::new()
    }
}

pgsync::process_global! {
    static LEDGER: pgsync::Mutex<Ledger> = pgsync::Mutex::new(Ledger::new());
}

fn kind_name(k: JobKind) -> &'static str {
    match k {
        JobKind::Concat => "concat",
        JobKind::SortedMerge => "sorted",
        JobKind::DvRewrite => "dv",
        JobKind::Run => "run",
    }
}

fn csv<const N: usize>(v: &[u64; N]) -> String {
    let mut out: Vec<String> = Vec::with_capacity(N);
    out.extend(v.iter().map(|x| x.to_string()));
    out.join(",")
}

impl Ledger {
    pub const fn new() -> Ledger {
        Ledger { seq: 0, map: BTreeMap::new(), sweeps: 0, sweep_us: 0 }
    }

    fn touch(&mut self, db: Oid, relid: Oid) -> &mut Entry {
        self.seq += 1;
        let k = (db, relid);
        if self.map.len() >= TABLES && !self.map.contains_key(&k) {
            let old = self.map.iter().min_by_key(|(_, e)| e.seq).map(|(k, _)| *k);
            if let Some(old) = old {
                self.map.remove(&old);
            }
        }
        let seq = self.seq;
        let e = self.map.entry(k).or_insert_with(|| Entry::new(seq));
        e.seq = seq;
        e
    }

    pub fn record(&mut self, db: Oid, relid: Oid, kind: JobKind, reason: Reason, cost: &Cost) {
        let e = self.touch(db, relid);
        let v = cost.values();
        let s = &mut e.sums[reason as usize];
        s.0 += 1;
        for (a, x) in s.1.iter_mut().zip(v) {
            *a += x;
        }
        if e.last.len() == LAST {
            e.last.remove(0);
        }
        e.last.push(Done { kind, reason, cost: v });
    }

    pub fn shadow_job(&mut self, db: Oid, relid: Oid, scored: bool, skip: [bool; 2], first: Option<[f64; 2]>) {
        let e = self.touch(db, relid);
        for (set, &sk) in e.sh.iter_mut().zip(&skip) {
            set[SCORED] += u64::from(scored);
            set[SKIP] += u64::from(sk);
        }
        if first.is_some() {
            e.first = first;
        }
    }

    pub fn shadow_idle(&mut self, db: Oid, relid: Oid, n: u64, add: [u64; 2]) {
        if n == 0 && !self.map.contains_key(&(db, relid)) {
            return;
        }
        let e = self.touch(db, relid);
        for (set, &a) in e.sh.iter_mut().zip(&add) {
            set[SCORED] += n;
            set[ADD] += a;
            set[ADD_NOW] = a;
        }
    }

    pub fn shadow_cold(&mut self, db: Oid, relid: Oid) {
        self.touch(db, relid).cold += 1;
    }

    pub fn shadow_pass_end(&mut self, db: Oid, relids: &[Oid]) {
        let firsts: Vec<(Oid, [f64; 2])> = relids.iter().filter_map(|&r| self.map.get_mut(&(db, r)).and_then(|e| e.first.take()).map(|f| (r, f))).collect();
        for set in 0..2 {
            let mut order: Vec<usize> = (0..firsts.len()).collect();
            order.sort_by(|&a, &b| firsts[a].1[set].total_cmp(&firsts[b].1[set]));
            for (place, &i) in order.iter().enumerate() {
                if place != i {
                    if let Some(e) = self.map.get_mut(&(db, firsts[i].0)) {
                        e.sh[set][REORDER] += 1;
                    }
                }
            }
        }
    }

    pub fn sweep_done(&mut self, us: u64) {
        self.sweeps += 1;
        self.sweep_us += us;
    }

    pub fn text(&self, db: Oid, relid: Oid) -> String {
        let e = self.map.get(&(db, relid));
        let jobs: u64 = e.map_or(0, |e| e.sums.iter().map(|s| s.0).sum());
        let mut out = format!("led.fields={} led.jobs={jobs}", FIELDS.join(","));
        for r in Reason::ALL {
            let (n, s) = e.map_or((0, [0; 9]), |e| e.sums[r as usize]);
            out.push_str(&format!(" led.{}={n},{}", r.name(), csv(&s)));
        }
        let last = e.map_or(String::new(), |e| {
            let mut v: Vec<String> = Vec::with_capacity(LAST);
            v.extend(e.last.iter().rev().map(|d| format!("{}:{}:{}", kind_name(d.kind), d.reason.name(), csv(&d.cost))));
            v.join(";")
        });
        out.push_str(&format!(" led.last={last}"));
        let sh = e.map_or([[0; 5]; 2], |e| e.sh);
        out.push_str(&format!(" sh.fields={} sh.g={} sh.r={} sh.cold={} sh.sweeps={} sh.sweep_us={}", SH_FIELDS.join(","), csv(&sh[0]), csv(&sh[1]), e.map_or(0, |e| e.cold), self.sweeps, self.sweep_us));
        out
    }
}

pub fn record(db: Oid, relid: Oid, kind: JobKind, reason: Reason, cost: &Cost) {
    pgsync::lock(&LEDGER).record(db, relid, kind, reason, cost);
}

pub fn shadow_job(db: Oid, relid: Oid, scored: bool, skip: [bool; 2], first: Option<[f64; 2]>) {
    pgsync::lock(&LEDGER).shadow_job(db, relid, scored, skip, first);
}

pub fn shadow_idle(db: Oid, relid: Oid, n: u64, add: [u64; 2]) {
    pgsync::lock(&LEDGER).shadow_idle(db, relid, n, add);
}

pub fn shadow_cold(db: Oid, relid: Oid) {
    pgsync::lock(&LEDGER).shadow_cold(db, relid);
}

pub fn shadow_pass_end(db: Oid, relids: &[Oid]) {
    pgsync::lock(&LEDGER).shadow_pass_end(db, relids);
}

pub fn sweep_done(us: u64) {
    pgsync::lock(&LEDGER).sweep_done(us);
}

pub fn text(db: Oid, relid: Oid) -> String {
    pgsync::lock(&LEDGER).text(db, relid)
}

pub fn thread_cpu_us() -> u64 {
    let r = pg_rusage::pg_rusage_init();
    let us = (r.ru_utime_sec + r.ru_stime_sec) * 1_000_000 + r.ru_utime_usec + r.ru_stime_usec;
    u64::try_from(us).unwrap_or(0)
}

pub fn thread_wal_bytes() -> u64 {
    transam_xlog_seams::wal_usage::is_installed().then(transam_xlog_seams::wal_usage::call).map_or(0, |u| u.wal_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cost(rows: u64) -> Cost {
        Cost { inputs: 4, live_in: 40 * rows, bytes_in: 9000, bytes_out: 8000, rows, dropped: 1, thread_cpu_us: 700, wal_bytes: 512, publish_us: 90 }
    }

    #[test]
    fn jobs_sum_by_reason_and_the_last_eight_stay_whole() {
        let mut l = Ledger::new();
        assert_eq!(
            l.text(5, 16400),
            format!(
                "led.fields={} led.jobs=0 led.retire=0,0,0,0,0,0,0,0,0,0 led.fanin=0,0,0,0,0,0,0,0,0,0 led.aged=0,0,0,0,0,0,0,0,0,0 led.dv_floor=0,0,0,0,0,0,0,0,0,0 led.forced=0,0,0,0,0,0,0,0,0,0 led.tiered=0,0,0,0,0,0,0,0,0,0 led.last= sh.fields={} sh.g=0,0,0,0,0 sh.r=0,0,0,0,0 sh.cold=0 sh.sweeps=0 sh.sweep_us=0",
                FIELDS.join(","),
                SH_FIELDS.join(",")
            )
        );
        for i in 0..10 {
            l.record(5, 16400, JobKind::Concat, if i < 9 { Reason::Fanin } else { Reason::Aged }, &cost(1000 + i));
        }
        l.record(5, 16401, JobKind::DvRewrite, Reason::DvFloor, &cost(5));
        let t = l.text(5, 16400);
        assert!(t.contains(" led.jobs=10 "), "{t}");
        let fanin_rows: u64 = (0..9).map(|i| 1000 + i).sum();
        assert!(t.contains(&format!(" led.fanin=9,36,{},81000,72000,{fanin_rows},9,6300,4608,810 ", 40 * fanin_rows)), "{t}");
        assert!(t.contains(" led.aged=1,4,40360,9000,8000,1009,1,700,512,90 "), "{t}");
        let last = t.split(" led.last=").nth(1).unwrap().split(" sh.fields=").next().unwrap();
        assert_eq!(last.split(';').count(), LAST);
        assert!(last.starts_with("concat:aged:4,40360,") && last.ends_with("concat:fanin:4,40080,9000,8000,1002,1,700,512,90"), "{last}");
        assert!(l.text(5, 16401).contains(" led.dv_floor=1,4,200,9000,8000,5,1,700,512,90 "));
        assert!(l.text(6, 16400).contains(" led.jobs=0 "), "another database's table");
    }

    #[test]
    fn run_jobs_sum_as_tiered_or_forced() {
        let mut l = Ledger::new();
        l.record(5, 16400, JobKind::Concat, Reason::Fanin, &cost(10));
        l.record(5, 16400, JobKind::Run, Reason::Tiered, &cost(64000));
        l.record(5, 16400, JobKind::Run, Reason::Forced, &cost(7));
        let t = l.text(5, 16400);
        assert!(t.contains(" led.jobs=3 "), "{t}");
        assert!(t.contains(&format!(" led.tiered=1,4,{},9000,8000,64000,1,700,512,90 led.last=", 40 * 64000)), "{t}");
        assert!(t.contains(" led.forced=1,4,280,9000,8000,7,1,700,512,90 "), "{t}");
        let last = t.split(" led.last=").nth(1).unwrap().split(" sh.fields=").next().unwrap();
        assert!(last.starts_with("run:forced:") && last.contains(";run:tiered:4,2560000,") && last.ends_with(";concat:fanin:4,400,9000,8000,10,1,700,512,90"), "{last}");
    }

    #[test]
    fn past_the_table_cap_the_table_written_longest_ago_goes() {
        let mut l = Ledger::new();
        for r in 0..TABLES as Oid {
            l.record(5, 20000 + r, JobKind::Concat, Reason::Fanin, &cost(1));
        }
        l.record(5, 20000, JobKind::Concat, Reason::Fanin, &cost(1));
        l.record(5, 20000 + TABLES as Oid, JobKind::Concat, Reason::Fanin, &cost(1));
        assert_eq!(l.map.len(), TABLES);
        assert!(l.text(5, 20000).contains(" led.jobs=2 "), "written again: kept");
        assert!(l.text(5, 20001).contains(" led.jobs=0 "), "the oldest write went");
    }

    #[test]
    fn the_shadow_counts_by_set_and_a_pass_out_of_payback_order_counts_a_reorder() {
        let mut l = Ledger::new();
        l.shadow_idle(5, 30000, 0, [0, 0]);
        assert!(l.map.is_empty(), "a table with nothing to score gets no entry");
        l.shadow_idle(5, 30000, 3, [2, 0]);
        l.shadow_idle(5, 30000, 2, [1, 1]);
        l.shadow_job(5, 30000, true, [false, true], None);
        l.shadow_cold(5, 30001);
        let t = l.text(5, 30000);
        assert!(t.contains(" sh.g=6,0,3,0,1 sh.r=6,1,1,0,1 sh.cold=0 "), "{t}");
        assert!(l.text(5, 30001).contains(" sh.cold=1 "));
        l.shadow_job(5, 30010, true, [false, false], Some([50.0, 1.0]));
        l.shadow_job(5, 30011, false, [false, false], Some([0.0, 2.0]));
        l.shadow_job(5, 30012, true, [false, false], Some([10.0, 3.0]));
        l.shadow_pass_end(5, &[30010, 30011, 30012, 30099]);
        let reorder = |l: &Ledger, r: Oid| [0, 1].map(|s| l.map[&(5, r)].sh[s][REORDER]);
        assert_eq!([reorder(&l, 30010), reorder(&l, 30011), reorder(&l, 30012)], [[1, 0], [1, 0], [1, 0]]);
        assert!(l.map[&(5, 30010)].first.is_none(), "cleared at the pass's end");
        l.shadow_pass_end(5, &[30010, 30011, 30012]);
        assert_eq!(reorder(&l, 30010), [1, 0], "no first job this pass: nothing counted");
        l.sweep_done(40);
        l.sweep_done(2);
        assert!(l.text(5, 30000).ends_with(" sh.sweeps=2 sh.sweep_us=42"));
    }
}

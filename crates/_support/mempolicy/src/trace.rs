//! One record per policy decision while `PGRUST_MEMPOLICY=trace=1`. A record's text is built only inside `on()`, so a
//! server without the trace never formats or allocates for it.

use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

static ON: AtomicBool = AtomicBool::new(false);
static SINK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

pub type Sink = fn(decision: &str, record: &str);

#[inline(always)]
pub fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

pub fn enable(sink: Sink) {
    SINK.store(sink as *mut (), Ordering::Release);
    ON.store(true, Ordering::Release);
    memledger::set_statement_end_hook(Some(statement_end));
}

pub fn disable() {
    ON.store(false, Ordering::Release);
    memledger::set_statement_end_hook(None);
}

#[cold]
fn statement_end(r: &memledger::StatementEnd) {
    if !on() {
        return;
    }
    let list = |peaks: &[i64; memledger::KINDS]| {
        let mut out = String::new();
        for (k, p) in memledger::Kind::ALL.iter().zip(peaks).filter(|(_, p)| **p > 0) {
            out.push_str(&format!("{}{}:{p}", if out.is_empty() { "" } else { "," }, k.name())); // alloc: Cold
        }
        if out.is_empty() {
            out.push('-');
        }
        out
    };
    let record = format!( // alloc: Cold
        "pid={} seq={} peak={} end={} heap_peak={} mapped_peak={} mapped={} pool_reserved_peak={} kinds={} mapped_kinds={}",
        r.pid,
        r.seq,
        r.peak,
        r.end,
        r.heap_peak,
        r.mapped_peak,
        r.mapped,
        r.pool_reserved_peak,
        list(&r.kinds),
        list(&r.mapped_kinds)
    );
    note("statement end", &record);
}

/// Call only under `on()`.
#[cold]
pub fn note(decision: &str, record: &str) {
    let p = SINK.load(Ordering::Acquire);
    if p.is_null() {
        return;
    }
    // SAFETY: `SINK` only ever holds a `Sink` stored by `enable`.
    let sink: Sink = unsafe { core::mem::transmute::<*mut (), Sink>(p) };
    sink(decision, record);
}

#[cfg(test)]
mod tests {
    use super::*;

    static SEEN: pgsync::Mutex<Vec<String>> = pgsync::Mutex::new(Vec::new());

    fn keep(decision: &str, record: &str) {
        pgsync::lock(&SEEN).push(format!("{decision}: {record}")); // alloc: Cold
    }

    #[test]
    fn records_reach_the_sink_only_while_on() {
        let _one = pgsync::lock(&crate::consts::TEST_LOCK);
        assert!(!on());
        enable(keep);
        assert!(on());
        note("admit.floor", "need 48 MiB past the lane: alone");
        disable();
        assert!(!on());
        assert_eq!(pgsync::lock(&SEEN).as_slice(), ["admit.floor: need 48 MiB past the lane: alone"]);
    }
}

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

static SHARED: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
static DRAINS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static ON: Cell<u8> = const { Cell::new(0) };
    static NAMES: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
    /// The names this thread put in `SHARED` since drain number `.0` (so a per-granule site locks once per statement).
    static SEEN: RefCell<(u64, Vec<&'static str>)> = const { RefCell::new((0, Vec::new())) };
}

#[inline]
pub fn on() -> bool {
    ON.with(|c| {
        if c.get() == 0 {
            c.set(if std::env::var_os("PGRUST_GRAVITY_REPORT").is_some() { 2 } else { 1 });
        }
        c.get() == 2
    })
}

pub fn set(on: bool) {
    ON.with(|c| c.set(if on { 2 } else { 1 }));
}

#[inline]
pub fn fire(name: &'static str) {
    if !on() {
        return;
    }
    NAMES.with(|n| {
        let mut n = n.borrow_mut();
        if !n.contains(&name) {
            n.push(name); // alloc: Cold (report knob on only)
        }
    });
}

#[inline]
pub fn fire_shared(name: &'static str) {
    if !on() {
        return;
    }
    SEEN.with(|s| {
        let mut s = s.borrow_mut();
        let d = DRAINS.load(Ordering::Acquire);
        if s.0 != d {
            *s = (d, Vec::new());
        }
        if s.1.contains(&name) {
            return;
        }
        s.1.push(name); // alloc: Cold (report knob on only)
        let mut g = SHARED.lock().unwrap_or_else(|e| e.into_inner());
        if !g.contains(&name) {
            g.push(name); // alloc: Cold (report knob on only)
        }
    });
}

pub fn drain() -> String {
    if !on() {
        return String::new();
    }
    let mut v = NAMES.with(|n| core::mem::take(&mut *n.borrow_mut()));
    {
        let mut g = SHARED.lock().unwrap_or_else(|e| e.into_inner());
        v.append(&mut g);
        DRAINS.fetch_add(1, Ordering::AcqRel);
    }
    v.sort_unstable();
    v.dedup();
    v.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    static TURN: Mutex<()> = Mutex::new(());

    #[test]
    fn records_once_sorted_and_drains() {
        let _t = TURN.lock().unwrap_or_else(|e| e.into_inner());
        set(true);
        fire("b.x");
        fire("a.y");
        fire("b.x");
        assert_eq!(drain(), "a.y,b.x");
        assert_eq!(drain(), "");
        set(false);
        fire("c");
        set(true);
        assert_eq!(drain(), "");
    }

    #[test]
    fn shared_fires_from_other_threads_reach_drain() {
        let _t = TURN.lock().unwrap_or_else(|e| e.into_inner());
        set(true);
        std::thread::spawn(|| {
            set(true);
            fire_shared("w.z");
            fire_shared("w.z");
        })
        .join()
        .unwrap();
        fire("a.y");
        fire_shared("a.y");
        assert_eq!(drain(), "a.y,w.z");
        std::thread::spawn(|| {
            set(true);
            fire_shared("w.z");
        })
        .join()
        .unwrap();
        assert_eq!(drain(), "w.z");
    }
}

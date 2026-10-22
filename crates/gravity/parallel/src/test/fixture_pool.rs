use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle, Thread};

use gravity_abi::MorselRange;

use crate::cancel::{ErrorFunnel, FirstError};
use crate::claim::MorselSource;
use crate::{Job, Pool};

struct Posted {
    claims: *const (dyn MorselSource + 'static),
    job: *const Job<'static>,
    width: u16,
    leader: Option<Thread>,
}

struct Shared {
    generation: Mutex<u64>,
    wake: Condvar,
    posted: UnsafeCell<Posted>,
    pending: AtomicUsize,
    claims_run: AtomicU64,
    funnel: ErrorFunnel,
}

unsafe impl Sync for Shared {}
unsafe impl Send for Shared {}

pub struct FixturePool {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

const STOP: u64 = u64::MAX;

impl FixturePool {
    pub fn new(threads: u16) -> FixturePool {
        let shared = Arc::new(Shared { // alloc: Cold
            generation: Mutex::new(0),
            wake: Condvar::new(),
            posted: UnsafeCell::new(Posted { claims: core::ptr::null::<NoClaims>() as *const dyn MorselSource, job: &noop_job as *const Job<'static>, width: 0, leader: None }),
            pending: AtomicUsize::new(0),
            claims_run: AtomicU64::new(0),
            funnel: ErrorFunnel::new(),
        });
        let handles = (0..threads).map(|ordinal| {
            let s = shared.clone();
            thread::spawn(move || serve(s, ordinal))
        });
        let threads = handles.collect(); // alloc: Cold
        FixturePool { shared, threads }
    }

    pub fn threads(&self) -> u16 {
        self.threads.len() as u16
    }

    pub fn claims_run(&self) -> u64 {
        self.shared.claims_run.load(Ordering::Relaxed)
    }
}

struct NoClaims;

impl MorselSource for NoClaims {
    fn total(&self) -> u64 {
        0
    }
    fn unit(&self) -> crate::claim::Unit {
        crate::claim::Unit::Block
    }
    fn claim(&self) -> Option<MorselRange> {
        None
    }
    fn close(&self) {}
    fn remaining(&self) -> u64 {
        0
    }
    fn reset(&self) {}
}

fn noop_job(_: u16, _: MorselRange) -> gravity_driver::Result<()> {
    Ok(())
}

fn serve(s: Arc<Shared>, ordinal: u16) {
    let mut seen = 0u64;
    loop {
        // the posting is read under the generation lock, with the generation it belongs to: read
        // after the lock, a worker descheduled here while a short generation completed would act on
        // the NEXT posting under the old `seen`, run that generation twice, and decrement `pending`
        // once too often — the count wraps and the leader parks for good (the t3 hang)
        let (generation, width, claims, job, leader) = {
            let mut g = s.generation.lock().unwrap_or_else(|e| e.into_inner());
            while *g == seen {
                g = s.wake.wait(g).unwrap_or_else(|e| e.into_inner());
            }
            let posted = unsafe { &*s.posted.get() };
            (*g, posted.width, posted.claims, posted.job, posted.leader.clone())
        };
        if generation == STOP {
            return;
        }
        seen = generation;
        if ordinal >= width {
            continue;
        }
        let claims = unsafe { &*claims };
        let job = unsafe { &*job };
        while let Some(range) = claims.claim() {
            s.claims_run.fetch_add(1, Ordering::Relaxed);
            if let Err(e) = job(ordinal, range) {
                s.funnel.offer(range.lo, ordinal, e);
                claims.close();
            }
        }
        if s.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
            if let Some(t) = leader {
                t.unpark();
            }
        }
    }
}

impl Pool for FixturePool {
    fn threads(&self) -> u16 {
        FixturePool::threads(self)
    }
    fn run(&self, width: u16, claims: &dyn MorselSource, job: &Job<'_>) -> Result<(), FirstError> {
        assert!(width >= 1 && width <= self.threads(), "width {width} on a pool of {}", self.threads());
        let s = &*self.shared;
        // the posting, the count and the generation change under one lock: the workers snapshot the
        // posting under the same lock (`serve`), so no worker can pair a generation with another's posting
        {
            let mut g = s.generation.lock().unwrap_or_else(|e| e.into_inner());
            unsafe {
                let p = &mut *s.posted.get();
                p.claims = core::mem::transmute::<*const (dyn MorselSource + '_), *const (dyn MorselSource + 'static)>(claims as *const dyn MorselSource);
                p.job = core::mem::transmute::<*const Job<'_>, *const Job<'static>>(job as *const Job<'_>);
                p.width = width;
                p.leader = Some(thread::current());
            }
            s.pending.store(width as usize, Ordering::Release);
            *g += 1;
            s.wake.notify_all();
        }
        while s.pending.load(Ordering::Acquire) != 0 {
            thread::park();
        }
        {
            let _g = s.generation.lock().unwrap_or_else(|e| e.into_inner());
            unsafe {
                let p = &mut *s.posted.get();
                p.width = 0;
                p.leader = None;
            }
        }
        match s.funnel.take() {
            Some(first) => Err(first),
            None => Ok(()),
        }
    }

    fn claims_run(&self) -> u64 {
        FixturePool::claims_run(self)
    }
}

impl Drop for FixturePool {
    fn drop(&mut self) {
        {
            let mut g = self.shared.generation.lock().unwrap_or_else(|e| e.into_inner());
            *g = STOP;
            self.shared.wake.notify_all();
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

//! The instance aux's auxiliary slot (one when enabled at start, else none) and the fence flag.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering::Relaxed};

static COUNT: AtomicI32 = AtomicI32::new(0);
static FENCED: AtomicBool = AtomicBool::new(false);

pub fn set_instance_aux_enabled(on: bool) {
    COUNT.store(on as i32, Relaxed);
}

#[inline]
pub fn instance_aux_count() -> i32 {
    COUNT.load(Relaxed)
}

pub fn set_fenced(on: bool) {
    FENCED.store(on, Relaxed);
}

#[inline]
pub fn fenced() -> bool {
    FENCED.load(Relaxed)
}

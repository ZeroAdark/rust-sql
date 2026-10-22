
use core::cell::Cell;
use core::sync::atomic::{AtomicU8, Ordering::Relaxed};

pub const CTX_EPQ: u8 = 1;
pub const CTX_SPI: u8 = 2;
pub const CTX_CURSOR: u8 = 4;

/// 0 = unresolved, 1 = disarmed (the default), 2 = armed.
static ARMED: AtomicU8 = AtomicU8::new(0);

#[inline]
pub fn armed() -> bool {
    match ARMED.load(Relaxed) {
        1 => false,
        2 => true,
        _ => armed_resolve(),
    }
}

#[cold]
#[inline(never)]
fn armed_resolve() -> bool {
    let on = matches!(
        std::env::var("PGRUST_P8_CENSUS").as_deref(),
        Ok("1") | Ok("on")
    );
    ARMED.store(if on { 2 } else { 1 }, Relaxed);
    on
}

pub fn arm_for_tests(on: bool) {
    ARMED.store(if on { 2 } else { 1 }, Relaxed);
}

thread_local! {
    static CTX: Cell<u8> = const { Cell::new(0) };
}

#[inline]
pub fn ctx_bits() -> u8 {
    CTX.with(|c| c.get())
}

pub struct CtxGuard {
    prev: u8,
    active: bool,
}

impl Drop for CtxGuard {
    fn drop(&mut self) {
        if self.active {
            CTX.with(|c| c.set(self.prev));
        }
    }
}

fn ctx_scope(bit: u8) -> CtxGuard {
    if !armed() {
        return CtxGuard { prev: 0, active: false };
    }
    let prev = CTX.with(|c| {
        let p = c.get();
        c.set(p | bit);
        p
    });
    CtxGuard { prev, active: true }
}

impl CtxGuard {
    pub fn is_active(&self) -> bool {
        self.active
    }
}

pub fn spi_scope() -> CtxGuard {
    ctx_scope(CTX_SPI)
}

pub fn cursor_scope() -> CtxGuard {
    ctx_scope(CTX_CURSOR)
}

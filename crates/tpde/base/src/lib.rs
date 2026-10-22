// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![doc = include_str!("../tests/ownership.md")]

mod val_local_idx;
pub use val_local_idx::ValLocalIdx;

#[cfg(test)]
#[path = "../tests/behavior.rs"]
mod tests;

pub use core::primitive::{i16, i32, i64, i8, u16, u32, u64, u8};

pub const WITH_ASSERTS: bool = cfg!(debug_assertions);
pub const LOGGING: bool = cfg!(feature = "logging");

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Level {
    Trace = 0,
    Debug = 1,
    Info = 2,
    Warn = 3,
    Error = 4,
    Critical = 5,
    Off = 6,
}

pub type LogSink = for<'a> fn(Level, core::fmt::Arguments<'a>);

#[derive(Clone, Copy)]
pub struct LogConfig {
    pub level: Level,
    pub sink: LogSink,
}

thread_local! {
    static LOG_CONFIG: core::cell::Cell<LogConfig> = const {
        core::cell::Cell::new(LogConfig { level: Level::Info, sink: stderr_sink })
    };
}

pub fn set_log_config(config: LogConfig) -> LogConfig {
    LOG_CONFIG.with(|current| current.replace(config))
}

pub fn set_log_level(level: Level) -> Level {
    LOG_CONFIG.with(|current| {
        let old = current.get();
        current.set(LogConfig { level, ..old });
        old.level
    })
}

#[inline]
pub fn should_log(level: Level) -> bool {
    LOGGING && LOG_CONFIG.with(|current| level >= current.get().level)
}

#[doc(hidden)]
pub fn emit_log(level: Level, message: core::fmt::Arguments<'_>) {
    if LOGGING {
        let config = LOG_CONFIG.with(core::cell::Cell::get);
        if level >= config.level {
            (config.sink)(level, message);
        }
    }
}

#[cold]
fn stderr_sink(level: Level, message: core::fmt::Arguments<'_>) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr().lock(), "[{level:?}] {message}");
}

#[cfg(feature = "logging")]
#[macro_export]
macro_rules! tpde_log {
    ($level:expr, $($args:tt)*) => {{
        if $crate::should_log($level) {
            $crate::emit_log($level, format_args!($($args)*));
        }
    }};
}

#[cfg(not(feature = "logging"))]
#[macro_export]
macro_rules! tpde_log {
    ($($args:tt)*) => {{}};
}

#[cfg(all(feature = "logging", debug_assertions))]
#[macro_export]
macro_rules! tpde_log_trace {
    ($($args:tt)*) => { $crate::tpde_log!($crate::Level::Trace, $($args)*); };
}

#[cfg(not(all(feature = "logging", debug_assertions)))]
#[macro_export]
macro_rules! tpde_log_trace {
    ($($args:tt)*) => {{}};
}

#[cfg(all(feature = "logging", debug_assertions))]
#[macro_export]
macro_rules! tpde_log_dbg {
    ($($args:tt)*) => { $crate::tpde_log!($crate::Level::Debug, $($args)*); };
}

#[cfg(not(all(feature = "logging", debug_assertions)))]
#[macro_export]
macro_rules! tpde_log_dbg {
    ($($args:tt)*) => {{}};
}

#[macro_export]
macro_rules! tpde_log_info {
    ($($args:tt)*) => { $crate::tpde_log!($crate::Level::Info, $($args)*); };
}

#[macro_export]
macro_rules! tpde_log_warn {
    ($($args:tt)*) => { $crate::tpde_log!($crate::Level::Warn, $($args)*); };
}

#[macro_export]
macro_rules! tpde_log_err {
    ($($args:tt)*) => { $crate::tpde_log!($crate::Level::Error, $($args)*); };
}

#[cold]
#[inline(never)]
#[track_caller]
pub fn fatal_error(message: &str) -> ! {
    tpde_log_err!("TPDE FATAL ERROR: {}", message);
    #[cfg(panic = "unwind")]
    panic!("{message}");
    #[cfg(panic = "abort")]
    std::process::abort();
}

#[macro_export]
macro_rules! tpde_fatal {
    ($message:expr) => {
        $crate::fatal_error($message)
    };
}

#[macro_export]
macro_rules! tpde_unreachable {
    ($message:expr) => {
        $crate::unreachable_error(stringify!($message))
    };
}

#[cold]
#[inline(never)]
#[doc(hidden)]
pub fn unreachable_error(expression: &str) -> ! {
    #[cfg(debug_assertions)]
    {
        use std::io::Write;
        let _ = writeln!(std::io::stderr().lock(), "TPDE unreachable: {expression}");
        std::process::abort();
    }
    #[cfg(not(debug_assertions))]
    panic!("TPDE unreachable: {expression}");
}

const _: () = {
    assert!(!core::mem::needs_drop::<LogConfig>());
    assert!(core::mem::size_of::<LogConfig>() <= 16);
};

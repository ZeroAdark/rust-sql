// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::*;
use std::{
    cell::{Cell, RefCell},
    mem::size_of,
};

thread_local! {
    static MESSAGES: RefCell<Vec<(Level, String)>> = const { RefCell::new(Vec::new()) };
}

fn collect(level: Level, message: std::fmt::Arguments<'_>) {
    MESSAGES.with(|messages| messages.borrow_mut().push((level, message.to_string())));
}

fn configure(level: Level) {
    MESSAGES.with(|messages| messages.borrow_mut().clear());
    set_log_config(LogConfig {
        level,
        sink: collect,
    });
}

#[test]
fn scalar_widths_and_signedness() {
    assert_eq!(
        [
            size_of::<u8>(),
            size_of::<u16>(),
            size_of::<u32>(),
            size_of::<u64>()
        ],
        [1, 2, 4, 8]
    );
    assert_eq!(
        [
            size_of::<i8>(),
            size_of::<i16>(),
            size_of::<i32>(),
            size_of::<i64>()
        ],
        [1, 2, 4, 8]
    );
    assert_eq!(i8::MIN, -128);
    assert_eq!(u64::MAX, 18_446_744_073_709_551_615);
    assert_eq!(WITH_ASSERTS, cfg!(debug_assertions));
    assert!(!std::mem::needs_drop::<LogConfig>());
}

#[test]
fn default_level_and_configuration_replacement() {
    let old = set_log_level(Level::Trace);
    assert_eq!(old, Level::Info);
    assert_eq!(set_log_level(Level::Warn), Level::Trace);
    let old = set_log_config(LogConfig {
        level: Level::Error,
        sink: collect,
    });
    assert_eq!(old.level, Level::Warn);
}

#[test]
fn every_threshold_and_level() {
    let levels = [
        Level::Trace,
        Level::Debug,
        Level::Info,
        Level::Warn,
        Level::Error,
        Level::Critical,
        Level::Off,
    ];
    for threshold in levels {
        set_log_level(threshold);
        for level in levels {
            assert_eq!(should_log(level), LOGGING && level >= threshold);
        }
    }
}

#[test]
fn suppressed_arguments_are_not_evaluated() {
    configure(Level::Off);
    let evaluated = Cell::new(0);
    tpde_log_info!("{}", {
        evaluated.set(evaluated.get() + 1);
        7
    });
    tpde_log_warn!("{}", {
        evaluated.set(evaluated.get() + 1);
        7
    });
    tpde_log_err!("{}", {
        evaluated.set(evaluated.get() + 1);
        7
    });
    assert_eq!(evaluated.get(), 0);
    MESSAGES.with(|messages| assert!(messages.borrow().is_empty()));
}

#[test]
fn debug_release_and_feature_gates() {
    configure(Level::Trace);
    let evaluated = Cell::new(0);
    tpde_log_trace!("trace:{}", {
        evaluated.set(evaluated.get() + 1);
        11
    });
    tpde_log_dbg!("debug:{}", {
        evaluated.set(evaluated.get() + 1);
        12
    });
    tpde_log_info!("info:{}", {
        evaluated.set(evaluated.get() + 1);
        13
    });
    tpde_log_warn!("warn:{}", {
        evaluated.set(evaluated.get() + 1);
        14
    });
    tpde_log_err!("error:{}", {
        evaluated.set(evaluated.get() + 1);
        15
    });
    assert_eq!(
        evaluated.get(),
        if LOGGING {
            if WITH_ASSERTS {
                5
            } else {
                3
            }
        } else {
            0
        }
    );
    MESSAGES.with(|messages| assert_eq!(messages.borrow().len(), evaluated.get()));
}

#[test]
fn generic_macro_matches_level_expression_evaluation() {
    configure(Level::Info);
    let evaluated = Cell::new(0);
    tpde_log!(
        {
            evaluated.set(evaluated.get() + 1);
            Level::Info
        },
        "hello"
    );
    assert_eq!(evaluated.get(), if LOGGING { 2 } else { 0 });
    evaluated.set(0);
    tpde_log!(
        {
            evaluated.set(evaluated.get() + 1);
            Level::Trace
        },
        "hidden"
    );
    assert_eq!(evaluated.get(), if LOGGING { 1 } else { 0 });
}

#[test]
fn sink_can_reenter_configuration() {
    fn reenter(level: Level, message: std::fmt::Arguments<'_>) {
        collect(level, message);
        set_log_level(Level::Off);
        tpde_log_info!("suppressed recursively");
    }
    set_log_config(LogConfig {
        level: Level::Info,
        sink: reenter,
    });
    tpde_log_info!("outer");
    MESSAGES.with(|messages| assert_eq!(messages.borrow().len(), usize::from(LOGGING)));
}

#[test]
fn configuration_is_backend_thread_local() {
    set_log_level(Level::Off);
    std::thread::spawn(|| assert_eq!(set_log_level(Level::Trace), Level::Info))
        .join()
        .unwrap();
    assert_eq!(set_log_level(Level::Warn), Level::Off);
}

#[cfg(panic = "unwind")]
#[test]
fn fatal_is_catchable_and_retains_message() {
    configure(Level::Off);
    let error = std::panic::catch_unwind(|| fatal_error("fatal detail"));
    let payload = error.unwrap_err();
    let text = payload.downcast_ref::<String>().unwrap();
    assert_eq!(text, "fatal detail");
}

#[cfg(all(panic = "unwind", not(debug_assertions)))]
#[test]
fn release_unreachable_does_not_evaluate_message() {
    let evaluated = Cell::new(false);
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tpde_unreachable!({
            evaluated.set(true);
            "unreachable detail"
        })))
        .is_err()
    );
    assert!(!evaluated.get());
}

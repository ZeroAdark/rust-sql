use std::mem::{align_of, size_of};
use tpde_base::*;

fn sink(_: Level, message: std::fmt::Arguments<'_>) {
    println!("{message}");
}

fn scalar<T>(signed: bool) {
    println!(
        "{}:{}:{}",
        size_of::<T>(),
        align_of::<T>(),
        u8::from(signed)
    );
}

fn normal() {
    scalar::<u8>(false);
    scalar::<u16>(false);
    scalar::<u32>(false);
    scalar::<u64>(false);
    scalar::<i8>(true);
    scalar::<i16>(true);
    scalar::<i32>(true);
    scalar::<i64>(true);
    println!("asserts:{}", u8::from(WITH_ASSERTS));
    for threshold in [
        Level::Trace,
        Level::Debug,
        Level::Info,
        Level::Warn,
        Level::Error,
        Level::Critical,
        Level::Off,
    ] {
        set_log_level(threshold);
        let mut evaluations = 0;
        println!("threshold:{}", threshold as u8);
        tpde_log_trace!("trace:{}", {
            evaluations += 1;
            evaluations
        });
        tpde_log_dbg!("debug:{}", {
            evaluations += 1;
            evaluations
        });
        tpde_log_info!("info:{}", {
            evaluations += 1;
            evaluations
        });
        tpde_log_warn!("warn:{}", {
            evaluations += 1;
            evaluations
        });
        tpde_log_err!("error:{}", {
            evaluations += 1;
            evaluations
        });
        tpde_log!(Level::Critical, "critical:{}", {
            evaluations += 1;
            evaluations
        });
        tpde_log!(Level::Off, "off:{}", {
            evaluations += 1;
            evaluations
        });
        println!("evaluations:{evaluations}");
    }
    let mut levels = 0;
    let mut values = 0;
    set_log_level(Level::Info);
    tpde_log!(
        {
            levels += 1;
            Level::Info
        },
        "generic:{}",
        {
            values += 1;
            values
        }
    );
    tpde_log!(
        {
            levels += 1;
            Level::Trace
        },
        "hidden:{}",
        {
            values += 1;
            values
        }
    );
    tpde_log_info!("mutate:{}", {
        set_log_level(Level::Off);
        values += 1;
        values
    });
    println!("generic-evaluations:{levels}:{values}");
    #[cfg(any(not(feature = "logging"), not(debug_assertions)))]
    {
        tpde_log_trace!(undefined_name_that_must_not_be_typechecked);
        tpde_log_dbg!(undefined_name_that_must_not_be_typechecked);
    }
    #[cfg(not(feature = "logging"))]
    {
        tpde_log_info!(undefined_name_that_must_not_be_typechecked);
        tpde_log_warn!(undefined_name_that_must_not_be_typechecked);
        tpde_log_err!(undefined_name_that_must_not_be_typechecked);
    }
    let mut a = 7;
    let mut b = 8;
    fn separate(a: &mut u32, b: &mut u32) {
        *a += 3;
        *b += 5;
    }
    separate(&mut a, &mut b);
    println!("separate:{a}:{b}");
}

fn main() {
    set_log_config(LogConfig {
        level: Level::Trace,
        sink,
    });
    std::panic::set_hook(Box::new(|_| {}));
    let mode = std::env::args().nth(1).unwrap_or_else(|| "normal".into());
    if mode == "normal" {
        normal();
        return;
    }
    if mode == "unreachable" {
        tpde_unreachable!("unreachable detail");
    }
    if mode == "fatal-off" {
        set_log_level(Level::Off);
    }
    #[cfg(panic = "unwind")]
    {
        let error = std::panic::catch_unwind(|| tpde_fatal!("fatal detail")).unwrap_err();
        println!("caught:{}", error.downcast_ref::<String>().unwrap());
    }
    #[cfg(panic = "abort")]
    tpde_fatal!("fatal detail");
}

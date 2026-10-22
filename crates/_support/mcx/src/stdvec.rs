//! Fallible sizing for std collections: a failed allocation is ERROR 53200 naming `ctx`, never an abort.
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use types_error::{PgError, PgResult};

#[cold]
fn oom<T>(ctx: &str, n: usize) -> Box<PgError> {
    crate::oom_boxed_named(ctx, n.saturating_mul(core::mem::size_of::<T>()))
}

pub fn try_vec<T>(n: usize, ctx: &str) -> PgResult<Vec<T>> {
    let mut v = Vec::new();
    v.try_reserve_exact(n).map_err(|_| oom::<T>(ctx, n))?;
    Ok(v)
}

pub fn try_filled<T: Clone>(n: usize, val: T, ctx: &str) -> PgResult<Vec<T>> {
    let mut v = try_vec(n, ctx)?;
    v.resize(n, val);
    Ok(v)
}

pub fn try_to_vec<T: Clone>(s: &[T], ctx: &str) -> PgResult<Vec<T>> {
    let mut v = try_vec(s.len(), ctx)?;
    v.extend_from_slice(s);
    Ok(v)
}

pub fn try_reserve_more<T>(v: &mut Vec<T>, extra: usize, ctx: &str) -> PgResult<()> {
    v.try_reserve(extra).map_err(|_| oom::<T>(ctx, extra))
}

pub fn try_resize<T: Clone>(v: &mut Vec<T>, n: usize, val: T, ctx: &str) -> PgResult<()> {
    if n > v.len() {
        try_reserve_more(v, n - v.len(), ctx)?;
    }
    v.resize(n, val);
    Ok(())
}

pub fn try_string(n: usize, ctx: &str) -> PgResult<String> {
    let mut s = String::new();
    s.try_reserve_exact(n).map_err(|_| oom::<u8>(ctx, n))?;
    Ok(s)
}

//! The one table of the memory policy's constants. `PGRUST_MEMPOLICY` (`<knob>=<int>`, `impl=<name>`, `trace=1`), read
//! once at server start, overrides them; an unknown key, a non-integer value or an unknown `impl` refuses the start.
//! Each knob's value and where it came from (`source`) read back in SQL (`pgrust_memory_settings`, gravity_hosts).

use core::sync::atomic::{AtomicU64, Ordering};

pub(crate) const MIB: u64 = 1 << 20;
pub(crate) const GIB: u64 = 1 << 30;

#[derive(Clone, Copy, Debug)]
pub struct Knob {
    pub name: &'static str,
    pub default: u64,
    pub doc: &'static str,
}

/// `knobs.rs`' list expands here: enum `K` in the list's order and `KNOBS`, one `Knob` per line.
macro_rules! knobs {
    ($($k:ident => $name:literal, $default:expr, $doc:literal;)*) => {
        /// The knobs, in [`KNOBS`]' order.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        #[repr(usize)]
        pub enum K {
            $($k,)*
        }

        pub const KNOBS: [Knob; [$($name),*].len()] = [$(Knob { name: $name, default: $default, doc: $doc },)*];
    };
}
pub(crate) use knobs;

pub use crate::knobs::{K, KNOBS};

pub const N: usize = KNOBS.len();

const fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

// knobs.rs merges by union: a knob added twice, or one with an empty name or doc, refuses the build here
const _: () = {
    let mut i = 0;
    while i < N {
        assert!(!KNOBS[i].name.is_empty() && !KNOBS[i].doc.is_empty(), "a knob with an empty name or doc (knobs.rs)");
        let mut j = i + 1;
        while j < N {
            assert!(!same(KNOBS[i].name, KNOBS[j].name), "a knob name twice (knobs.rs)");
            j += 1;
        }
        i += 1;
    }
};

static VALUES: [AtomicU64; N] = {
    let mut v = [const { AtomicU64::new(0) }; N];
    let mut i = 0;
    while i < N {
        v[i] = AtomicU64::new(KNOBS[i].default);
        i += 1;
    }
    v
};

/// Where a knob's value came from (the memory settings view's `source`): its default, `PGRUST_MEMPOLICY`'s text, or a
/// `set` by the process (a test, or the allocator's start reading `PGRUST_MEMPOLICY` before the policy's start applies it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Source {
    Default = 0,
    Env = 1,
    Set = 2,
}

impl Source {
    pub const fn name(self) -> &'static str {
        match self {
            Source::Default => "default",
            Source::Env => "PGRUST_MEMPOLICY",
            Source::Set => "set",
        }
    }
}

static SOURCES: [core::sync::atomic::AtomicU8; N] = [const { core::sync::atomic::AtomicU8::new(Source::Default as u8) }; N];

#[inline]
pub fn get(k: K) -> u64 {
    VALUES[k as usize].load(Ordering::Relaxed)
}

#[inline]
pub fn get_ms(k: K) -> core::time::Duration {
    core::time::Duration::from_millis(get(k))
}

pub fn set(k: K, value: u64) {
    VALUES[k as usize].store(value, Ordering::Relaxed);
    SOURCES[k as usize].store(Source::Set as u8, Ordering::Relaxed);
}

/// Where knob `k`'s value came from (the last of `reset`, `apply`, `set` to write it).
pub fn source(k: K) -> Source {
    source_at(k as usize)
}

fn source_at(i: usize) -> Source {
    match SOURCES[i].load(Ordering::Relaxed) {
        1 => Source::Env,
        2 => Source::Set,
        _ => Source::Default,
    }
}

/// Every knob with its value now and where it came from, in `KNOBS`' order (the memory settings view: a new knob is a
/// new row there with no other change).
pub fn each(mut f: impl FnMut(&Knob, u64, Source)) {
    for (i, k) in KNOBS.iter().enumerate() {
        f(k, VALUES[i].load(Ordering::Relaxed), source_at(i));
    }
}

pub fn reset() {
    for (i, k) in KNOBS.iter().enumerate() {
        VALUES[i].store(k.default, Ordering::Relaxed);
        SOURCES[i].store(Source::Default as u8, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Impl {
    Default = 0,
}

static IMPL: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(Impl::Default as u8);

#[inline]
pub fn rules() -> Impl {
    match IMPL.load(Ordering::Relaxed) {
        _ => Impl::Default,
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    pub knobs: u32,
    pub trace: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal<'a> {
    UnknownKey(&'a str),
    NotAnInteger(&'a str),
    UnknownImpl(&'a str),
}

/// All or nothing: the first bad entry refuses the whole value and sets nothing.
pub fn apply(spec: &str) -> Result<Applied, Refusal<'_>> {
    let entries = || spec.split(',').map(str::trim).filter(|e| !e.is_empty());
    let mut applied = Applied::default();
    for e in entries() {
        let (key, value) = e.split_once('=').map(|(k, v)| (k.trim(), v.trim())).ok_or(Refusal::UnknownKey(e))?;
        match key {
            "impl" => {
                if value != "default" {
                    return Err(Refusal::UnknownImpl(e));
                }
            }
            "trace" => {
                if value != "0" && value != "1" {
                    return Err(Refusal::NotAnInteger(e));
                }
            }
            _ => {
                if !KNOBS.iter().any(|k| k.name == key) {
                    return Err(Refusal::UnknownKey(e));
                }
                value.parse::<u64>().map_err(|_| Refusal::NotAnInteger(e))?;
            }
        }
    }
    for e in entries() {
        let (key, value) = e.split_once('=').map(|(k, v)| (k.trim(), v.trim())).expect("validated");
        match key {
            "impl" => IMPL.store(Impl::Default as u8, Ordering::Relaxed),
            "trace" => applied.trace = value == "1",
            _ => {
                let i = KNOBS.iter().position(|k| k.name == key).expect("validated");
                VALUES[i].store(value.parse().expect("validated"), Ordering::Relaxed);
                SOURCES[i].store(Source::Env as u8, Ordering::Relaxed);
                applied.knobs += 1;
            }
        }
    }
    Ok(applied)
}

#[doc(hidden)]
pub static TEST_LOCK: pgsync::Mutex<()> = pgsync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_in_the_enums_order_and_names_are_unique() {
        assert_eq!(KNOBS[K::OsReserveMin as usize].name, "os_reserve_min");
        assert_eq!(KNOBS[K::BytesWaitMs as usize].name, "bytes_wait_ms");
        assert_eq!(N, KNOBS.len());
        for (i, a) in KNOBS.iter().enumerate() {
            assert!(KNOBS[i + 1..].iter().all(|b| b.name != a.name), "{} twice", a.name);
            assert!(!a.doc.is_empty(), "{} has no doc", a.name);
        }
    }

    #[test]
    fn apply_sets_knobs_and_refuses_unknown_keys_bad_values_and_unknown_rules_whole() {
        let _one = pgsync::lock(&TEST_LOCK);
        reset();
        assert_eq!(get(K::BytesWaitMs), 2_000);
        assert_eq!(apply(" bytes_wait_ms = 500 , trace=1,impl=default"), Ok(Applied { knobs: 1, trace: true }));
        assert_eq!(get(K::BytesWaitMs), 500);
        assert_eq!(get_ms(K::BytesWaitMs), core::time::Duration::from_millis(500));
        assert_eq!(apply("work_pool_min=1,ghost=3"), Err(Refusal::UnknownKey("ghost=3")));
        assert_eq!(get(K::WorkPoolMin), GIB, "a refusal sets nothing");
        assert_eq!(apply("work_pool_min=1G"), Err(Refusal::NotAnInteger("work_pool_min=1G")));
        assert_eq!(apply("impl=spark_fair"), Err(Refusal::UnknownImpl("impl=spark_fair")));
        assert_eq!(apply("trace=yes"), Err(Refusal::NotAnInteger("trace=yes")));
        assert_eq!(apply("bytes_wait_ms"), Err(Refusal::UnknownKey("bytes_wait_ms")));
        assert_eq!(apply(""), Ok(Applied::default()));
        assert_eq!(rules(), Impl::Default);
        reset();
        assert_eq!(get(K::BytesWaitMs), 2_000);
    }

    /// Criterion 11 (the memory settings view): each knob's source is the last writer's, and a refused text writes none.
    #[test]
    fn each_knob_records_where_its_value_came_from() {
        let _one = pgsync::lock(&TEST_LOCK);
        reset();
        assert_eq!([source(K::CacheFloor), source(K::GiveBackSpill), source(K::BytesWaitMs)], [Source::Default; 3]);
        assert_eq!(apply("cache_floor=1, give_back_spill=1"), Ok(Applied { knobs: 2, trace: false }));
        assert_eq!((get(K::CacheFloor), source(K::CacheFloor)), (1, Source::Env));
        assert_eq!(source(K::GiveBackSpill), Source::Env, "a text that names the default value still sets it");
        assert_eq!(source(K::BytesWaitMs), Source::Default);
        assert_eq!(apply("bytes_wait_ms=5,ghost=1"), Err(Refusal::UnknownKey("ghost=1")));
        assert_eq!(source(K::BytesWaitMs), Source::Default, "a refusal sets no source");
        let mut seen = 0;
        each(|k, v, s| {
            seen += 1;
            if k.name == "cache_floor" {
                assert_eq!((v, s), (1, Source::Env));
            }
        });
        assert_eq!(seen, N, "every knob once");
        set(K::CacheFloor, 0);
        assert_eq!((get(K::CacheFloor), source(K::CacheFloor)), (0, Source::Set));
        assert_eq!([Source::Default.name(), Source::Env.name(), Source::Set.name()], ["default", "PGRUST_MEMPOLICY", "set"]);
        reset();
        assert_eq!(source(K::CacheFloor), Source::Default);
    }
}

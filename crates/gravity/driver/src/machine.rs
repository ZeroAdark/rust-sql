//! The machine profile: the L1d, L2 and SMT facts the partitioned-record constants were
//! measured against, detected once (Linux sysfs; anything else reads as the reference case). Graviton (c8g: 2 MiB L2
//! per core, no SMT) is the reference case: there every derived value is BYTE-IDENTICAL to the constant it replaces
//! (`sink::CACHE_PARTITION_BYTES`, `breaker::FOLD_PF`); c6a (512 KiB L2 shared by two SMT threads) gets L2/2 and a
//! shorter prefetch ring. Overrides: `PGRUST_GRAVITY_PARTITION_BYTES` (sink), `PGRUST_GRAVITY_PREFETCH`; disclosed
//! once on stderr under `PGRUST_GRAVITY_PHASES` (`machine: l1d=… l2=… smt=… cores=… threads=… isa=… ram=…`). `ram_bytes` is
//! `/proc/meminfo` MemTotal (the reference's `REFERENCE_RAM` where it does not read): the RAM fractions of the cost home
//! (the code cache's budget, gap-optoverfit KN-07) take it from here.

use pgsync::OnceLock;

pub const REFERENCE_L1D: u64 = gravity_program::cost::Machine::REFERENCE.l1d;
pub const REFERENCE_L2: u64 = gravity_program::cost::Machine::REFERENCE.l2;
pub const REFERENCE_PREFETCH: usize = gravity_program::cost::Machine::REFERENCE.prefetch;
const SMT_PREFETCH: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MachineProfile {
    /// One core's level-1 data cache (Graviton: 64 KiB).
    pub l1d_bytes: u64,
    pub l2_bytes: u64,
    pub smt: u32,
    pub cores: u32,
    pub threads: u32,
    /// `neon`, `sve2`, `sse2`, `avx2` or `avx512`.
    pub isa: &'static str,
    /// L2 ÷ SMT siblings (Graviton: 2 MiB).
    pub partition_bytes: u64,
    /// The fold ring's depth, ≤ `breaker::FOLD_PF`.
    pub prefetch: usize,
    /// The machine's RAM, bytes (MemTotal).
    pub ram_bytes: u64,
}

impl MachineProfile {
    pub fn reference() -> MachineProfile {
        // sysfs first: a backend under a cgroup or an affinity mask sees `available_parallelism` = 1 (c6a: 1 of 16)
        let threads = sysfs_online().or_else(|| std::thread::available_parallelism().ok().map(|n| n.get() as u32)).unwrap_or(1);
        MachineProfile { l1d_bytes: REFERENCE_L1D, l2_bytes: REFERENCE_L2, smt: 1, cores: threads, threads, isa: isa(), partition_bytes: REFERENCE_L2, prefetch: REFERENCE_PREFETCH, ram_bytes: REFERENCE_RAM }
    }

    fn detect() -> MachineProfile {
        let mut m = MachineProfile::reference();
        // D06: the cache facts are the cost home's one host reading (`cost::machine::host`), the planner's too
        let host = gravity_program::cost::machine::host();
        if let Some(l1d) = gravity_program::cost::machine::host_l1d() {
            m.l1d_bytes = l1d;
        }
        if gravity_program::cost::machine::host_detected() {
            m.l2_bytes = host.l2;
        }
        if let Some(ram) = ram_reading() {
            m.ram_bytes = ram;
        }
        m.smt = host.smt.max(1);
        m.cores = (m.threads / m.smt).max(1);
        m.partition_bytes = (m.l2_bytes / m.smt as u64).max(64 << 10);
        m.prefetch = if m.smt > 1 { SMT_PREFETCH } else { REFERENCE_PREFETCH };
        if let Some(pf) = crate::env::num::<usize>("PGRUST_GRAVITY_PREFETCH") {
            m.prefetch = pf.clamp(1, REFERENCE_PREFETCH);
        }
        m
    }

    /// The cost home's facts for this machine (`gravity_program::cost::Machine`): what a size or span derives from.
    pub fn cost(&self) -> gravity_program::cost::Machine {
        gravity_program::cost::Machine::detected(Some(self.l1d_bytes), self.l2_bytes, self.smt, self.prefetch)
    }

    pub fn describe(&self) -> String {
        format!("l1d={}K l2={}K smt={} cores={} threads={} isa={} partition={}K prefetch={} ram={}M", self.l1d_bytes >> 10, self.l2_bytes >> 10, self.smt, self.cores, self.threads, self.isa, self.partition_bytes >> 10, self.prefetch, self.ram_bytes >> 20) // alloc: Cold
    }
}

pub fn profile() -> &'static MachineProfile {
    static ONCE: OnceLock<MachineProfile> = OnceLock::new();
    ONCE.get_or_init(|| {
        let m = MachineProfile::detect();
        if crate::env::set("PGRUST_GRAVITY_PHASES") {
            eprintln!("machine: {}", m.describe());
        }
        // D06: an undetected L2 sizes every cache-derived table as the reference machine does; said once per process
        if !gravity_program::cost::machine::host_detected() {
            eprintln!("gravity: machine: the L2 size was not detected (sysfs cpu0/cache); the reference 2 MiB stands in (phase-1 table {} KiB)", gravity_program::cost::hash::t1_table_bytes(gravity_program::cost::machine::host()) >> 10);
        }
        m
    })
}

fn isa() -> &'static str {
    #[cfg(target_arch = "aarch64")]
    {
        if std::arch::is_aarch64_feature_detected!("sve2") {
            "sve2"
        } else {
            "neon"
        }
    }
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx512f") {
            "avx512"
        } else if is_x86_feature_detected!("avx2") {
            "avx2"
        } else {
            "sse2"
        }
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        "scalar"
    }
}





/// The reference box's RAM (c8g.4xlarge: 16 threads, 32 GiB).
pub const REFERENCE_RAM: u64 = 32 << 30;

/// The memory this server may use, bytes: the memory limit the memory watchdog published (`mcx::governor::limit`, D3314:
/// `pgrust.memory_watchdog_limit`, else the smallest cgroup memory.max over the process's cgroup and its ancestors, else
/// physical memory; published at postmaster start, before any backend), never past `/proc/meminfo` MemTotal; MemTotal
/// alone where nothing is published (no watchdog: unit tests). The grant pool, the part cache, the resident tables, the
/// build pool, the condition cache, the zone store and the code cache are all shares of it: under a 16 GiB container on a
/// 61 GiB box the pool armed from MemTotal at 45 GiB and the kernel OOM-killed the server. D3175: wasm32 has no /proc;
/// there the linear memory the host declares in `PGRUST_WASM_MEMORY` (bytes), else wasm32's whole 4 GiB address space.
pub fn ram_reading() -> Option<u64> {
    if cfg!(target_family = "wasm") {
        return Some(crate::env::num::<u64>("PGRUST_WASM_MEMORY").filter(|&b| b > 0).unwrap_or(1 << 32));
    }
    limited(mcx::governor::limit() as u64, meminfo_total())
}

/// The published limit (0 = none) against MemTotal.
fn limited(limit: u64, total: Option<u64>) -> Option<u64> {
    match (limit, total) {
        (0, t) => t,
        (l, Some(t)) => Some(l.min(t)),
        (l, None) => Some(l),
    }
}

/// `/proc/meminfo` MemTotal, bytes.
fn meminfo_total() -> Option<u64> {
    meminfo_line(&std::fs::read_to_string("/proc/meminfo").ok()?) // alloc: Cold
}

fn meminfo_line(text: &str) -> Option<u64> {
    let kib: u64 = text.lines().find_map(|l| l.strip_prefix("MemTotal:"))?.split_whitespace().next()?.parse().ok()?;
    (kib > 0).then_some(kib << 10)
}


/// `cpu/online` (`0-15`): the machine's hardware threads, whatever this process's affinity mask.
fn sysfs_online() -> Option<u32> {
    count_list(std::fs::read_to_string("/sys/devices/system/cpu/online").ok()?.trim())
}

fn count_list(s: &str) -> Option<u32> {
    let mut n = 0u32;
    for part in s.split(',') {
        n += match part.split_once('-') {
            Some((a, b)) => b.parse::<u32>().ok()? - a.parse::<u32>().ok()? + 1,
            None => 1,
        };
    }
    Some(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_program::cost::machine::parse_size;

    #[test]
    fn sizes_parse() {
        assert_eq!(parse_size("2048K"), Some(2 << 20));
        assert_eq!(parse_size("512K"), Some(512 << 10));
        assert_eq!(parse_size("1M"), Some(1 << 20));
        assert_eq!(parse_size("x"), None);
    }

    #[test]
    fn lists_count() {
        assert_eq!((count_list("0-15"), count_list("0,8"), count_list("0-1,4-5")), (Some(16), Some(2), Some(4)));
    }

    #[test]
    fn memtotal_parses() {
        assert_eq!(meminfo_line("MemTotal:       32505856 kB\nMemFree: 1 kB\n"), Some(32505856 << 10));
        assert_eq!((meminfo_line("MemFree: 1 kB\n"), meminfo_line("MemTotal: 0 kB\n")), (None, None));
    }

    /// D3314: the memory a server sizes its auto caps from is the published limit (a cgroup's), never past MemTotal;
    /// MemTotal where nothing is published.
    #[test]
    fn d3314_ram_is_the_published_limit_under_memtotal() {
        const G: u64 = 1 << 30;
        assert_eq!(limited(16 * G, Some(61 * G)), Some(16 * G));
        assert_eq!(limited(64 * G, Some(61 * G)), Some(61 * G));
        assert_eq!(limited(0, Some(61 * G)), Some(61 * G));
        assert_eq!((limited(8 * G, None), limited(0, None)), (Some(8 * G), None));
    }

    #[test]
    fn reference_is_the_constants() {
        let m = MachineProfile::reference();
        assert_eq!((m.partition_bytes, m.prefetch, m.smt, m.ram_bytes), (REFERENCE_L2, REFERENCE_PREFETCH, 1, REFERENCE_RAM));
        assert_eq!(m.l1d_bytes, 64 << 10, "Graviton 4's L1d, the cost home's reference");
    }
}

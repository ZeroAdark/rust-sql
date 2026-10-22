
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Machine {
    pub l1d: u64,
    pub l2: u64,
    pub line: u64,
    pub smt: u32,
    pub prefetch: usize,
}

pub(crate) const L2_SHARE_FLOOR: u64 = 64 << 10;

impl Machine {
    pub const REFERENCE: Machine = Machine { l1d: 64 << 10, l2: 2 << 20, line: 64, smt: 1, prefetch: 16 };

    /// A detected machine: the driver's L2, SMT siblings and prefetch ring, and L1d when it was read (else the reference's).
    pub const fn detected(l1d: Option<u64>, l2: u64, smt: u32, prefetch: usize) -> Machine {
        let l1d = match l1d {
            Some(b) if b > 0 => b,
            _ => Machine::REFERENCE.l1d,
        };
        Machine { l1d, l2, line: Machine::REFERENCE.line, smt: if smt == 0 { 1 } else { smt }, prefetch: if prefetch == 0 { 1 } else { prefetch } }
    }

    pub const fn l2_share(&self) -> u64 {
        let s = self.l2 / self.smt as u64;
        if s < L2_SHARE_FLOOR {
            L2_SHARE_FLOOR
        } else {
            s
        }
    }

    pub const fn l1d_share(&self) -> u64 {
        self.l1d / self.smt as u64
    }

    /// The entries of `item` bytes that fill `num / den` of `cache` bytes, at least one: a window's span (a window refills,
    /// so its span bounds locality, never what the operator can do).
    pub const fn span(cache: u64, item: u64, num: u64, den: u64) -> u64 {
        let item = if item == 0 { 1 } else { item };
        let den = if den == 0 { 1 } else { den };
        let n = cache / den * num / item;
        if n == 0 {
            1
        } else {
            n
        }
    }

    pub const fn l1_span(&self, item: u64, num: u64, den: u64) -> u64 {
        Machine::span(self.l1d_share(), item, num, den)
    }

    pub const fn l2_span(&self, item: u64, num: u64, den: u64) -> u64 {
        Machine::span(self.l2_share(), item, num, den)
    }
}

/// The cache facts every size and span derives from: this host's (Linux sysfs, cpu0, detected once per process), or the
/// reference machine's where the process asks for the reference profile (`anchor::word`: PGRUST_COST_PROFILE=reference, a
/// debug build), so a rig or an A/B arm sizes as the reference does on any host. The ONE reading both the planner (its
/// Gravity paths' phase-1 sizes) and the run time (the driver's machine profile, abi's tables) take, so plan cost and run
/// time never disagree. `None` facts read as the reference's; `host_detected` says whether the L2 was read.
pub fn host() -> &'static Machine {
    if crate::anchor::word().is_reference() {
        &Machine::REFERENCE
    } else {
        &host_facts().0
    }
}

pub fn host_detected() -> bool {
    crate::anchor::word().is_reference() || host_facts().1
}

pub fn host_l1d() -> Option<u64> {
    if crate::anchor::word().is_reference() {
        Some(Machine::REFERENCE.l1d)
    } else {
        host_facts().2
    }
}

/// This host's cache facts as sysfs reads them, whatever the profile: the machine, whether its L2 was read, its L1d when
/// read (the cost profile's host key).
pub fn sysfs() -> (&'static Machine, bool, Option<u64>) {
    let f = host_facts();
    (&f.0, f.1, f.2)
}

fn host_facts() -> &'static (Machine, bool, Option<u64>) {
    static ONCE: std::sync::OnceLock<(Machine, bool, Option<u64>)> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        let (l1d, l2, smt) = (sysfs_cache(1, "Data"), sysfs_cache(2, ""), sysfs_smt());
        let m = Machine::detected(l1d, l2.unwrap_or(Machine::REFERENCE.l2), smt.unwrap_or(1).max(1), if smt.unwrap_or(1) > 1 { SMT_PREFETCH } else { Machine::REFERENCE.prefetch });
        (m, l2.is_some(), l1d)
    })
}

pub const SMT_PREFETCH: usize = 8;

pub fn sysfs_cache(level: u32, kind: &str) -> Option<u64> {
    for i in 0..8 {
        let dir = format!("/sys/devices/system/cpu/cpu0/cache/index{i}");
        let read = |name: &str| std::fs::read_to_string(format!("{dir}/{name}")).ok();
        let at = read("level")?;
        if at.trim().parse::<u32>().ok() != Some(level) || (!kind.is_empty() && read("type")?.trim() != kind) {
            continue;
        }
        return parse_size(read("size")?.trim());
    }
    None
}

pub fn sysfs_smt() -> Option<u32> {
    count_list(std::fs::read_to_string("/sys/devices/system/cpu/cpu0/topology/thread_siblings_list").ok()?.trim())
}

pub fn count_list(s: &str) -> Option<u32> {
    let mut n = 0u32;
    for part in s.split(',') {
        n += match part.split_once('-') {
            Some((a, b)) => b.parse::<u32>().ok()? - a.parse::<u32>().ok()? + 1,
            None => 1,
        };
    }
    Some(n)
}

pub fn parse_size(s: &str) -> Option<u64> {
    let (num, unit) = s.split_at(s.trim_end_matches(|c: char| c.is_ascii_alphabetic()).len());
    let n: u64 = num.parse().ok()?;
    Some(match unit {
        "K" | "k" => n << 10,
        "M" | "m" => n << 20,
        "" => n,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reference_derives_the_constants_it_replaces() {
        let m = Machine::REFERENCE;
        assert_eq!((m.l2_share(), m.l1d_share(), m.prefetch), (2 << 20, 64 << 10, 16));
        assert_eq!(m.l1_span(8, 1, 2), 4096);
    }

    #[test]
    fn an_smt_machine_shares_its_caches() {
        let m = Machine::detected(Some(32 << 10), 512 << 10, 2, 8);
        assert_eq!((m.l2_share(), m.l1d_share(), m.prefetch), (256 << 10, 16 << 10, 8));
        assert_eq!(Machine::detected(None, 64 << 10, 4, 0).l2_share(), L2_SHARE_FLOOR);
        assert_eq!(Machine::detected(None, 2 << 20, 0, 16).l1d, Machine::REFERENCE.l1d);
    }

    #[test]
    fn a_span_is_at_least_one_entry() {
        assert_eq!((Machine::span(64, 128, 1, 1), Machine::span(1 << 20, 0, 1, 0)), (1, 1 << 20));
    }
}

//! cpuset: the CPU set the pool is sized from and pinned to (lane numapool,
//! 2026-09-15). Two rules, both from the metal-48xl per-socket A/B
//! (docs/gravity/hosts.md "CPU affinity"): (1) the pool never has more
//! workers than the CPUs it may run on — the allowed set is the process's
//! `Cpus_allowed_list` (cpusets, `taskset`, containers) intersected with
//! `PGRUST_CPU_AFFINITY_SET` when given; (2) on a machine with more than one
//! NUMA node the pool lives on ONE node — its workers are that node's
//! allowed CPUs and (launch_backend cpuaff) are pinned there — because a
//! statement's fold/owner traffic spread over two sockets was the worst arm
//! measured (hot 0.81x vs 0.66x on one socket). A single-node machine
//! is unchanged: the plan is the boot mask.
//!
//! Knobs (env, exact spelling): `PGRUST_RUNTIME_NUMA` = `node` (default) |
//! `per-node` (ONE POOL PER NODE: N runtimes, each sized to its node's
//! allowed CPUs and pinned there; a statement is placed whole on the node
//! with the fewest statements in flight — `gravity_parallel::place`) |
//! `all` (every allowed CPU, the pre-2026-09-15 map) | `off` (no topology
//! read; same effect as `all`); `PGRUST_RUNTIME_NUMA_NODE=<id>` picks the
//! node (default: the lowest-numbered node holding an allowed CPU — a
//! deterministic choice, so two boots of one box pin the same way);
//! `PGRUST_RUNTIME_WORKERS` still overrides the count explicitly (the
//! cpuaff e2e's wrap arm relies on that); `PGRUST_CPU_AFFINITY=0` turns
//! pinning off, and an unpinned pool is sized to every allowed CPU (a node
//! without a pin is meaningless). An explicit `PGRUST_CPU_AFFINITY_SET`
//! beats the node rule: the operator's list is taken as given.
//!
//! Computed once per process (the postmaster, before the pool spawns) and
//! read by the runtime's sizing and by cpuaff; pure functions underneath
//! so the policy is unit-testable without a machine.

use pgsync::OnceLock;

/// cpu_set_t width (CPU_SETSIZE): the hard upper bound on nameable cores.
pub const CPU_SET_WIDTH: usize = 1024;

/// One NUMA node in sysfs order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub id: usize,
    pub cpus: Vec<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumaMode {
    Node,
    PerNode,
    All,
    Off,
}

impl NumaMode {
    fn from_env() -> NumaMode {
        match std::env::var("PGRUST_RUNTIME_NUMA").as_deref() {
            Ok("per-node") => NumaMode::PerNode,
            Ok("all") => NumaMode::All,
            Ok("off") => NumaMode::Off,
            _ => NumaMode::Node,
        }
    }
}

/// The plan: the pool's CPU set (ascending) and how it was chosen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CpuPlan {
    /// The CPUs the (first) pool may use: the sizing cap and the pin set.
    pub cpus: Vec<usize>,
    /// Every pool's CPU set, by pool index (`pools[0] == cpus`); more than
    /// one only under `per-node` on a multi-node machine.
    pub pools: Vec<Vec<usize>>,
    /// The chosen NUMA node (id, node count) when the node rule applied.
    pub node: Option<(usize, usize)>,
    /// Whether `PGRUST_CPU_AFFINITY_SET` was honoured (explicit list).
    pub explicit_set: bool,
    /// Whether pinning is requested (`PGRUST_CPU_AFFINITY` != "0").
    pub pinned: bool,
    /// Boot-time notes for the witness (set narrowed, node picked, ...).
    pub notes: Vec<String>,
}

impl CpuPlan {
    /// The worker cap: never more workers than CPUs in the plan.
    pub fn workers_cap(&self) -> usize {
        self.cpus.len().max(1)
    }

    /// The CPU set for the page pool's helper threads (`pagepool::set_helper_cpus`): the workers' CPUs (every pool's)
    /// when the NUMA node rule narrowed them to fewer than the `boot` mask, else None (one node, `all` / `off`,
    /// `per-node` spanning every node, an explicit `PGRUST_CPU_AFFINITY_SET`, pinning off: the helpers keep the process
    /// mask). The pool's claimer prefaults most of the pool's memory, and a page lands on the node of the thread that
    /// first touches it: an unpinned claimer on the other node left the workers' memory remote.
    pub fn helper_cpus(&self, boot: &[usize]) -> Option<Vec<usize>> {
        if !self.pinned || self.node.is_none() {
            return None;
        }
        let mut set: Vec<usize> = self.pools.iter().flatten().copied().filter(|c| boot.contains(c)).collect(); // alloc: Cold
        set.sort_unstable();
        set.dedup();
        (!set.is_empty() && set.len() < boot.len()).then_some(set)
    }
}

static PLAN: OnceLock<CpuPlan> = OnceLock::new();

/// The process plan, computed on first use from the environment and the
/// machine (`/proc/self/status`, `/sys/devices/system/node`).
pub fn plan() -> &'static CpuPlan {
    PLAN.get_or_init(|| {
        let boot = allowed_cpus();
        let set = std::env::var("PGRUST_CPU_AFFINITY_SET").ok();
        let node_override = std::env::var("PGRUST_RUNTIME_NUMA_NODE").ok().and_then(|v| v.trim().parse().ok());
        select(&boot, set.as_deref(), pinning_requested(), NumaMode::from_env(), &topology(), node_override)
    })
}

/// Whether pinning is armed: ON unless `PGRUST_CPU_AFFINITY` is exactly "0".
pub fn pinning_requested() -> bool {
    std::env::var("PGRUST_CPU_AFFINITY").as_deref() != Ok("0")
}

/// The plan when the boot-time buffers cannot be allocated: unpinned, no explicit set.
fn oom_plan(pinned: bool) -> CpuPlan {
    let _ = pinned;
    CpuPlan {
        cpus: Vec::new(),
        pools: Vec::new(),
        node: None,
        explicit_set: false,
        pinned: false,
        notes: Vec::new(),
    }
}

/// The policy, pure: `boot` = the process's allowed CPUs (ascending),
/// `set` = the raw `PGRUST_CPU_AFFINITY_SET`, `nodes` = the topology (empty
/// on a single-node or non-Linux box).
pub fn select(
    boot: &[usize],
    set: Option<&str>,
    pinned: bool,
    mode: NumaMode,
    nodes: &[Node],
    node_override: Option<usize>,
) -> CpuPlan {
    let mut notes = Vec::new();
    let mut explicit_set = false;
    let mut cpus: Vec<usize> = Vec::new();
    if cpus.try_reserve_exact(boot.len()).is_err() {
        return oom_plan(pinned);
    }
    cpus.extend_from_slice(boot);
    if let Some(raw) = set {
        match parse_cpu_list(raw) {
            None => notes.push(format!("PGRUST_CPU_AFFINITY_SET ({raw:?}) is unparseable; using the boot mask")),
            Some(req) => {
                let inter: Vec<usize> = req.iter().copied().filter(|c| boot.contains(c)).collect();
                if inter.is_empty() {
                    notes.push(format!("PGRUST_CPU_AFFINITY_SET ({raw:?}) has no cores inside the boot mask; using the boot mask"));
                } else {
                    if inter.len() < req.len() {
                        notes.push(format!(
                            "PGRUST_CPU_AFFINITY_SET names cores outside the boot mask; narrowed to {}",
                            format_cpu_list(&inter)
                        ));
                    }
                    cpus = inter;
                    explicit_set = true;
                }
            }
        }
    }
    let mut node = None;
    // per-node: one pool per node that holds an allowed cpu (pinned, no
    // explicit list, more than one such node); the first pool is node 0's.
    if pinned && !explicit_set && mode == NumaMode::PerNode && nodes.len() > 1 {
        let mut pools: Vec<Vec<usize>> = Vec::new();
        if pools.try_reserve_exact(nodes.len()).is_err() {
            return oom_plan(pinned);
        }
        for n in nodes {
            let mut p: Vec<usize> = Vec::new();
            if p.try_reserve_exact(cpus.len()).is_err() {
                return oom_plan(pinned);
            }
            p.extend(cpus.iter().copied().filter(|c| n.cpus.contains(c)));
            if !p.is_empty() {
                pools.push(p);
            }
        }
        if pools.len() > 1 {
            let first = pools[0].clone();
            let id = nodes.iter().find(|n| n.cpus.contains(&first[0])).map_or(0, |n| n.id);
            return CpuPlan { cpus: first, node: Some((id, nodes.len())), pools, explicit_set, pinned, notes };
        }
    }
    // The node rule: only a pinned pool without an explicit list, on a
    // machine with more than one node.
    if pinned && !explicit_set && matches!(mode, NumaMode::Node | NumaMode::PerNode) && nodes.len() > 1 {
        let pick = node_override
            .and_then(|id| nodes.iter().find(|n| n.id == id && n.cpus.iter().any(|c| cpus.contains(c))))
            .or_else(|| nodes.iter().find(|n| n.cpus.iter().any(|c| cpus.contains(c))));
        if let Some(n) = pick {
            if node_override.is_some_and(|id| id != n.id) {
                notes.push(format!("PGRUST_RUNTIME_NUMA_NODE names a node with no allowed cpus; using node {}", n.id));
            }
            cpus.retain(|c| n.cpus.contains(c));
            node = Some((n.id, nodes.len()));
        }
    }
    CpuPlan { pools: vec![cpus.clone()], cpus, node, explicit_set, pinned, notes }
}

/// The calling process's allowed CPUs (Linux: `/proc/self/status`
/// `Cpus_allowed_list`, the same source the cpuaff e2e reads); elsewhere
/// `0..available_parallelism`.
pub fn allowed_cpus() -> Vec<usize> {
    let fallback = || (0..std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)).collect();
    #[cfg(target_os = "linux")]
    {
        if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
            for line in s.lines() {
                if let Some(rest) = line.strip_prefix("Cpus_allowed_list:") {
                    if let Some(v) = parse_cpu_list(rest.trim()) {
                        return v;
                    }
                }
            }
        }
    }
    fallback()
}

/// The NUMA topology (Linux sysfs `node<N>/cpulist`, ascending by id);
/// empty when unreadable or non-Linux — the caller treats empty as one node.
pub fn topology() -> Vec<Node> {
    #[cfg(target_os = "linux")]
    {
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir("/sys/devices/system/node") {
            for e in rd.flatten() {
                let name = e.file_name();
                let name = name.to_string_lossy();
                let Some(id) = name.strip_prefix("node").and_then(|s| s.parse::<usize>().ok()) else { continue };
                let Ok(list) = std::fs::read_to_string(e.path().join("cpulist")) else { continue };
                if let Some(cpus) = parse_cpu_list(list.trim()) {
                    out.push(Node { id, cpus });
                }
            }
        }
        out.sort_by_key(|n| n.id);
        return out;
    }
    #[allow(unreachable_code)]
    Vec::new()
}

/// Parse a core list: comma-separated cores and inclusive "a-b" ranges
/// ("1-14", "0,2,4", "0-3,8-11"); whitespace around items tolerated.
/// Output ascending + deduped. None on any malformed item, an empty list,
/// a reversed range, or a core >= the cpu_set_t width.
pub fn parse_cpu_list(s: &str) -> Option<Vec<usize>> {
    let mut out: Vec<usize> = Vec::new();
    for item in s.split(',') {
        let item = item.trim();
        if let Some((lo, hi)) = item.split_once('-') {
            let (lo, hi) = (lo.trim().parse::<usize>().ok()?, hi.trim().parse::<usize>().ok()?);
            if lo > hi {
                return None;
            }
            out.extend(lo..=hi);
        } else {
            out.push(item.parse::<usize>().ok()?);
        }
    }
    out.sort_unstable();
    out.dedup();
    if out.is_empty() || *out.last().unwrap() >= CPU_SET_WIDTH {
        return None;
    }
    Some(out)
}

/// Render an ascending core list in the kernel's Cpus_allowed_list shape
/// ("0-3,8").
pub fn format_cpu_list(cpus: &[usize]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < cpus.len() {
        let start = cpus[i];
        let mut end = start;
        while i + 1 < cpus.len() && cpus[i + 1] == end + 1 {
            i += 1;
            end = cpus[i];
        }
        if !out.is_empty() {
            out.push(',');
        }
        if start == end {
            out.push_str(&start.to_string());
        } else {
            out.push_str(&format!("{start}-{end}"));
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_nodes() -> Vec<Node> {
        vec![Node { id: 0, cpus: (0..96).collect() }, Node { id: 1, cpus: (96..192).collect() }]
    }

    #[test]
    fn single_node_is_the_boot_mask() {
        let boot: Vec<usize> = (0..16).collect();
        let p = select(&boot, None, true, NumaMode::Node, &[], None);
        assert_eq!(p.cpus, boot);
        assert_eq!(p.node, None);
        assert_eq!(p.workers_cap(), 16);
    }

    #[test]
    fn two_nodes_pick_node_zero_and_cap_to_it() {
        let boot: Vec<usize> = (0..192).collect();
        let p = select(&boot, None, true, NumaMode::Node, &two_nodes(), None);
        assert_eq!(p.cpus, (0..96).collect::<Vec<_>>());
        assert_eq!(p.node, Some((0, 2)));
        assert_eq!(p.workers_cap(), 96);
    }

    #[test]
    fn node_override_and_fallback() {
        let boot: Vec<usize> = (0..192).collect();
        let p = select(&boot, None, true, NumaMode::Node, &two_nodes(), Some(1));
        assert_eq!(p.cpus, (96..192).collect::<Vec<_>>());
        assert_eq!(p.node, Some((1, 2)));
        // A node with no allowed cpus falls back to the lowest that has some.
        let boot: Vec<usize> = (0..96).collect();
        let p = select(&boot, None, true, NumaMode::Node, &two_nodes(), Some(1));
        assert_eq!(p.node, Some((0, 2)));
        assert_eq!(p.notes.len(), 1);
    }

    #[test]
    fn explicit_set_beats_the_node_rule() {
        let boot: Vec<usize> = (0..192).collect();
        let p = select(&boot, Some("0-95,96-191"), true, NumaMode::Node, &two_nodes(), None);
        assert_eq!(p.cpus, boot);
        assert!(p.explicit_set);
        assert_eq!(p.node, None);
    }

    #[test]
    fn unpinned_or_all_spans_every_allowed_cpu() {
        let boot: Vec<usize> = (0..192).collect();
        assert_eq!(select(&boot, None, false, NumaMode::Node, &two_nodes(), None).cpus, boot);
        assert_eq!(select(&boot, None, true, NumaMode::All, &two_nodes(), None).cpus, boot);
        assert_eq!(select(&boot, None, true, NumaMode::Off, &two_nodes(), None).cpus, boot);
    }

    #[test]
    fn per_node_makes_one_pool_per_node() {
        let boot: Vec<usize> = (0..192).collect();
        let p = select(&boot, None, true, NumaMode::PerNode, &two_nodes(), None);
        assert_eq!(p.pools.len(), 2);
        assert_eq!(p.pools[0], (0..96).collect::<Vec<_>>());
        assert_eq!(p.pools[1], (96..192).collect::<Vec<_>>());
        assert_eq!(p.cpus, p.pools[0]);
        assert_eq!(p.workers_cap(), 96);
        // One node only allowed (a cpuset): per-node collapses to the node rule.
        let boot: Vec<usize> = (0..96).collect();
        let p = select(&boot, None, true, NumaMode::PerNode, &two_nodes(), None);
        assert_eq!(p.pools.len(), 1);
        assert_eq!(p.node, Some((0, 2)));
        // Single-node machine: identical to the default.
        let boot: Vec<usize> = (0..16).collect();
        assert_eq!(select(&boot, None, true, NumaMode::PerNode, &[], None), select(&boot, None, true, NumaMode::Node, &[], None));
    }

    #[test]
    fn cpuset_container_caps_the_pool() {
        // taskset -c 0-7 on a 16-core box: the boot mask is 0-7.
        let boot: Vec<usize> = (0..8).collect();
        let p = select(&boot, None, true, NumaMode::Node, &[], None);
        assert_eq!(p.workers_cap(), 8);
        // An explicit set outside the mask narrows loudly, never widens.
        let p = select(&boot, Some("4-11"), true, NumaMode::Node, &[], None);
        assert_eq!(p.cpus, vec![4, 5, 6, 7]);
        assert_eq!(p.notes.len(), 1);
    }

    #[test]
    fn helper_cpus_are_the_worker_node_and_none_when_the_workers_span_the_mask() {
        let boot: Vec<usize> = (0..192).collect();
        // the node rule on two nodes: the helpers go where the workers are
        let p = select(&boot, None, true, NumaMode::Node, &two_nodes(), None);
        assert_eq!(p.helper_cpus(&boot), Some((0..96).collect()));
        let p = select(&boot, None, true, NumaMode::Node, &two_nodes(), Some(1));
        assert_eq!(p.helper_cpus(&boot), Some((96..192).collect()));
        // every allowed CPU (all / off / unpinned / per-node over both nodes): nothing to narrow
        for mode in [NumaMode::All, NumaMode::Off, NumaMode::PerNode] {
            assert_eq!(select(&boot, None, true, mode, &two_nodes(), None).helper_cpus(&boot), None, "{mode:?}");
        }
        assert_eq!(select(&boot, None, false, NumaMode::Node, &two_nodes(), None).helper_cpus(&boot), None, "unpinned");
        // one node: the plan is the boot mask
        let one = vec![Node { id: 0, cpus: (0..16).collect() }];
        let b16: Vec<usize> = (0..16).collect();
        assert_eq!(select(&b16, None, true, NumaMode::Node, &one, None).helper_cpus(&b16), None);
        assert_eq!(select(&b16, None, true, NumaMode::Node, &[], None).helper_cpus(&b16), None);
        // an explicit list is the operator's: the helpers are left alone
        assert_eq!(select(&boot, Some("0-13"), true, NumaMode::Node, &two_nodes(), None).helper_cpus(&boot), None);
        // a container that allows part of each node: the workers' part of node 0
        let part: Vec<usize> = (40..140).collect();
        let p = select(&part, None, true, NumaMode::Node, &two_nodes(), None);
        assert_eq!(p.helper_cpus(&part), Some((40..96).collect()));
    }

    #[test]
    fn parse_and_format() {
        assert_eq!(parse_cpu_list("1-14"), Some((1..=14).collect()));
        assert_eq!(parse_cpu_list(" 0-3, 8 - 11 ,15"), Some(vec![0, 1, 2, 3, 8, 9, 10, 11, 15]));
        assert_eq!(parse_cpu_list("4,2,2,3,3-5"), Some(vec![2, 3, 4, 5]));
        for bad in ["", " ", "a", "1,,2", "1-", "-3", "3-1", "1;2", "0-1023,1024", "1024"] {
            assert_eq!(parse_cpu_list(bad), None, "input {bad:?}");
        }
        assert_eq!(format_cpu_list(&[0, 1, 3, 4, 7]), "0-1,3-4,7");
        for set in [vec![0, 1, 2, 3], vec![0, 2, 4], vec![1, 2, 3, 8, 9, 15]] {
            assert_eq!(parse_cpu_list(&format_cpu_list(&set)), Some(set));
        }
    }
}

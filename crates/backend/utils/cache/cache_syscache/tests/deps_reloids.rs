//! C3: gravity_deps' copy of every syscache's catalog (a catalog scan's dependencies) is `cacheinfo`'s.
#[test]
fn deps_syscache_reloids_match_cacheinfo() {
    let ours: Vec<u32> = cache_syscache::CACHEINFO.iter().map(|c| c.reloid).collect();
    assert_eq!(ours, gravity_deps::SYSCACHE_RELOIDS.to_vec());
    for (id, list) in [(cache_syscache::AMOPOPID, 3), (cache_syscache::AMOPSTRATEGY, 4), (cache_syscache::AMPROCNUM, 5), (cache_syscache::CLAOID, 14), (cache_syscache::OPEROID, 40), (cache_syscache::OPFAMILYOID, 42)] {
        assert_eq!(id, list);
        assert!(gravity_deps::SUPPORT_LISTS.contains(&(id as i16)));
    }
}

//! Partition write faces (docs/gravity/ladder/decompose-writes.md need 1): each target's column map
//! into the routing root (`AppendRelInfo.parent_colnos`, composed) and the root's write site.

use gravity_program::{ArenaVec, SiteDesc};

use crate::ledger::Kind;
use crate::admit::Refusal;
use crate::walk::Builder;
use crate::ops::shared::modifytable::{RouteDesc, Target};

/// `to_root[a - 1]` = the root attno of column `a` (0 = none); the root keeps the empty identity map.
pub fn faces<'p>(b: &Builder<'p, '_>, targets: &mut [Target<'p>], route: Option<RouteDesc>, k: Kind, id: i32) -> Result<(), Refusal> {
    let Some(r) = route else { return Ok(()) };
    // D3681: each child's AppendRelInfo found once, by range table index (its first, as `carry::root_attno` finds it),
    // not searched for again over every one for each column of each target
    // (in the arena: a warm decompose allocates nothing on the heap)
    let mut infos = ArenaVec::with_capacity_in(b.stmt.appendRelations.len(), b.arena); // alloc: Program
    infos.extend(b.stmt.appendRelations.iter().filter_map(|n| n.as_append_rel_info()).enumerate().map(|(i, a)| (a.child_relid, i, a)));
    infos.sort_unstable_by_key(|x| (x.0, x.1));
    let info = |rti: u32| infos.get(infos.partition_point(|x| x.0 < rti)).filter(|x| x.0 == rti).map(|x| x.2);
    // `carry::root_attno` over `info`: column `a` of `rti` up its parents' column maps to the routing root, `None` when a
    // level has no map or the column no parent column
    let root_attno = |mut rti: u32, mut a: i16| -> Option<i16> {
        while rti != r.root_rti {
            let ari = info(rti)?;
            a = ari.parent_colnos.get(a.checked_sub(1)? as usize).copied().filter(|&p| p > 0)?;
            rti = ari.parent_relid;
        }
        Some(a)
    };
    for t in targets.iter_mut() {
        if t.rti == r.root_rti {
            continue;
        }
        let natts = t.site.natts() as i16;
        let mut map = ArenaVec::with_capacity_in(natts.max(0) as usize, b.arena);
        let mut mapped = false;
        for a in 1..=natts {
            let ra = root_attno(t.rti, a).unwrap_or(0);
            if ra < 0 || ra as u16 > r.root_natts {
                return Err(Refusal::shape(k, id, "a target column mapped outside the routing root"));
            }
            mapped |= ra != 0;
            map.push(ra);
        }
        if !mapped && natts > 0 {
            return Err(Refusal::shape(k, id, "a target under a routing root without its AppendRelInfo column map"));
        }
        t.to_root = map;
    }
    Ok(())
}

/// The root's write site: a routed insert or a cross-partition re-insert forms its row here.
pub fn root_site(r: &RouteDesc) -> SiteDesc {
    SiteDesc::write(r.root_relid, r.root_natts)
}

pub fn root_attno(t: &Target<'_>, a: i16) -> i16 {
    if t.to_root.is_empty() {
        a
    } else {
        t.to_root.get(a.checked_sub(1).map_or(usize::MAX, |i| i as usize)).copied().unwrap_or(0)
    }
}

pub fn identity(t: &Target<'_>) -> bool {
    t.to_root.iter().enumerate().all(|(i, &ra)| ra == i as i16 + 1)
}

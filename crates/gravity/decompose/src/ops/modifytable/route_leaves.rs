//! The routed write's leaf set (docs/gravity/ladder/decompose-writes.md need 1): every leaf under the routing root as the
//! facts source knows it at admission — the consumer admits the whole tree before a row is written; the key moves with it.

use gravity_program::{ArenaVec, Cmd};

use crate::catalog::LeafFact;
use crate::walk::Builder;
use crate::ops::shared::modifytable::RouteDesc;

/// The tree's leaves for an INSERT / UPDATE / DELETE, `known` = the source answered (fixtures: no). A cross-partition
/// UPDATE's are its moved rows' destinations (H-e (c)); a MERGE's none (its move is M4-MERGE's).
pub fn leaves<'p>(b: &Builder<'p, '_>, route: Option<&RouteDesc>, cmd: Cmd) -> (ArenaVec<LeafFact>, bool) {
    let mut out = ArenaVec::new_in(b.arena);
    let Some(r) = route else { return (out, false) };
    // V4R-498: a MERGE's routed NOT MATCHED INSERTs read the leaves as an INSERT's (lowerv4 write.route's MERGE form);
    // MOVE: a MERGE whose UPDATE action may move the row reads them as an UPDATE's (write.route's `Route::moves`)
    if r.cross_partition && !matches!(cmd, Cmd::Update | Cmd::Merge) {
        return (out, false);
    }
    let known = b.facts.partition_leaves(r.root_relid, &mut |leaf| out.push(leaf));
    (out, known)
}

/// A leaf that is a foreign table (foreign tables are not supported: no Foreign Scan, no foreign write): exec's admission
/// raises 0A000 for a write this reason refuses (exec admit/removed.rs `leaf_refusal`), never a fallback.
pub const FOREIGN_LEAF: &str = "a partition that is a foreign table (foreign tables are not supported)";

pub fn unservable(l: &LeafFact) -> Option<&'static str> {
    if l.foreign {
        Some(FOREIGN_LEAF)
    } else if !l.heap {
        Some("a partition that is not a heap table (a foreign or other-AM leaf)")
    } else if let Some(what) = l.trig.refusal(Cmd::Insert).filter(|_| l.has_triggers) {
        Some(what)
    } else if l.has_rules {
        Some("a partition with rules")
    } else if !l.indexes_plain {
        Some("a partition with an index the lowered write cannot maintain (not a core access method, a non-btree unique without exclusion operators, not valid or ready, or a deferrable exclusion constraint)")
    } else {
        None
    }
}

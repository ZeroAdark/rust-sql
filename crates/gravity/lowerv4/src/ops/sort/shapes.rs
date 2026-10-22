//! Props: the sort roads' shapes and form G (V4R-118; one predicate per road, read by props for admission and by the road's
//! own file: runs/sort.rs, keylane.rs, keyed/emit_group.rs). M3-SORT.
use gravity_abi::breaker::KeyKind;
use gravity_decompose::ops::shared::agg::GroupDecl;
use gravity_decompose::ops::shared::sort::{SortDesc, SortForm, SortKey};
use gravity_decompose::{Op, Source};

use crate::graph::{emits_groups, Gx};

/// Road sort.keylane's shape: a full sort whose first key is one order word (an integer, date, timestamp or float key),
/// so its (flags, word) pair is the whole key the radix orders; or (D1582, G11) a text / varchar / bpchar key whose
/// word is a prefix of its order: its first 8 bytes under a collation that orders by bytes (`KeyColl.bytes_order`:
/// pg_locale's own `collate_is_c`, read by the hosts' facts), its sort key's under an ICU collation while key.coll is
/// on (rep.varlena.coll's `m6_coll_prefix`, rep/varkey.rs `coll_prefix`); a bpchar's word is taken over its payload
/// stripped of trailing blanks (rep.varlena.hash `payload`, as `bpchartruelen`). The radix orders the pairs and the
/// full row compare every tie (the image tail, the later keys), as it does a tie of two equal words. A numeric
/// mantissa, a code, a fingerprint, a key its own btree
/// family orders, or a text key under a libc collation (word 0: nothing for the radix) is not.
pub(crate) fn lane_key(d: &SortDesc<'_>, group: Option<&GroupDecl<'_>>) -> bool {
    let word = |k: &SortKey| matches!(k.kind, KeyKind::I32 | KeyKind::I64 | KeyKind::Date | KeyKind::Timestamp | KeyKind::F32 | KeyKind::F64) && (k.image.is_raw() || k.image.is_float());
    let prefix = |k: &SortKey| (k.image.is_text() || k.image.is_bpchar()) && !k.own_family && (k.coll.bytes_order || crate::rep::varkey::coll_prefix(k.coll.oid));
    // USEROP: a CompareFn key's word is the constant 0 (its comparator decides), never the whole key a radix orders by
    matches!(d.form, SortForm::Runs) && group.is_none() && d.keys.first().is_some_and(|k| !k.compare_fn() && (word(k) || prefix(k)))
}

/// Form G (runs/sort.rs, keyed/emit_group.rs): pipeline `k`'s full sort orders the records of the keyed group it drains
/// (agg_sort; no row is copied). It holds when the sort is `SortForm::Runs`, the drained group emits groups, only filters
/// and projections run before the sort (each arrival is one record), and neither slot is produced again between its life
/// and the sort's reader (D295: the records must stay live through the drain), no other pipeline drains the group
/// (`Gx::drained_twice`: its gather refiles the one emit state the drain re-reads), and every projected column resolves to
/// the group's row (A14 a: `Gx::projects_group_row`; else the sort orders the rows by value, `runs.sort`). Never in a
/// nest's or SubPlan's rerun set (D1339 row C: the drain's resumed gather reads consumed region 0, which an embedded inner
/// reading it does not have): a row sort there; a recursive union's term set (`RECURSIVE_SET`) keeps it.
pub(crate) fn group_sort(gx: &Gx<'_, '_>, k: usize) -> bool {
    let p = &gx.pipelines()[k];
    let (Some(g), Some(br)) = (gx.drained_group(k), crate::frame::iface::breaker_of(&p.desc)) else { return false };
    let Some(s) = p.ops.iter().position(|op| matches!(op, Op::Sort(_))) else { return false };
    let Op::Sort(d) = &p.ops[s] else { return false };
    let Source::Breaker { slot } = p.source else { return false };
    let reader = (k + 1..gx.pipelines().len()).find(|&j| matches!(gx.pipelines()[j].source, Source::Breaker { slot } if slot == br.slot));
    let again = |slot: u8, from: usize| reader.is_none_or(|r| (from + 1..=r).any(|m| gx.pipelines()[m].desc.produces() == Some(slot)));
    let life = gx.life_of(k, slot).map_or(k, |l| l.0 as usize);
    // USEROP: a CompareFn key (another family's comparator) orders rows by value through its comparator (runs.sort), never
    // the group's records by their key images
    let images = d.keys.iter().all(|key| !key.compare_fn());
    images && matches!(d.form, SortForm::Runs) && d.presorted == 0 && emits_groups(g) && p.ops[..s].iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_)))
        && !again(slot, life) && !again(br.slot, k) && !gx.drained_twice(k) && !crate::effects::visible::sort::finals_once(gx, g) && !crate::rerun::sets::in_owner_set(gx, k) && gx.projects_group_row(k)
}

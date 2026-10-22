//! NEW-numeric-1: an order-key layout (abi `LAYOUT_KEY_TIES`: every `MinI64` / `MaxI64` slot holds a numeric order key,
//! `value << 4 | dscale`) never takes the generated owner fold. Its owner fold is the generic one (`Rec::fold_newer` over
//! the partition read oldest first), which takes the newer member of a tie at two dscales, never a merge that keeps
//! the smaller word.

use gravity_abi::breaker::{KeyKind, RecordLayout, SlotKind, LAYOUT_KEY_TIES};
use gravity_kernels::table::owner_fold::generated;

#[test]
fn an_order_key_layout_takes_no_generated_owner_fold() {
    let slots = [SlotKind::MinI64, SlotKind::MaxI64, SlotKind::MaxI64, SlotKind::Count];
    for keys in [&[KeyKind::I64][..], &[KeyKind::I64, KeyKind::I64][..]] {
        let plain = RecordLayout::new(keys, &slots);
        assert!(generated(&plain), "the generated fold folds this layout ({} key words)", keys.len());
        let mut keyed = plain;
        keyed.flags |= LAYOUT_KEY_TIES;
        assert!(!generated(&keyed), "an order-key layout folds through the generic owner fold");
    }
}

//! A `RecordLayout` packed into its `LAYOUT_WORDS` parameter words (`words`) reads back equal (`from_words`), in both
//! tiers: the owner phases and the group descriptors carry layouts this way.
use gravity_abi::breaker::{KeyKind, RecordLayout, SlotKind, WideLayout, LAYOUT_WORDS};

#[test]
fn a_layout_reads_back_from_its_words() {
    let narrow = RecordLayout::new(&[KeyKind::I64, KeyKind::Code], &[SlotKind::Count, SlotKind::SumI128, SlotKind::MinCode]);
    let keys: &'static [KeyKind] = &[KeyKind::I32];
    let slots: &'static [SlotKind] = Box::leak(vec![SlotKind::SumI64; 40].into_iter().chain([SlotKind::Count]).collect::<Vec<_>>().into_boxed_slice());
    let offs: &'static mut [u32] = Box::leak(vec![0u32; slots.len()].into_boxed_slice());
    let w: &'static WideLayout = Box::leak(Box::new(WideLayout::compute(keys, slots, offs, true)));
    let wide = RecordLayout::wide(w, true, false);
    for l in [narrow, wide] {
        let words: [u64; LAYOUT_WORDS] = l.words();
        // SAFETY: `words()` of a live layout (the wide tier leaked for the test's life).
        let back = unsafe { RecordLayout::from_words(words) };
        assert_eq!(back, l);
        assert_eq!((back.size(), back.is_wide()), (l.size(), l.is_wide()));
    }
}
